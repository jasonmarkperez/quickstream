//! The Telemetry screen chrome: its building blocks (colour depth and palette in `ui::theme`; key
//! row, volume meter, progress bar and text fitting in `ui::kit`; width tiers in `ui::screen`) and
//! the chrome `AppUI::render` draws with them (background, header, tab row, strip, key row).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use common::fake_player::FakePlayer;
use common::tui;
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::kit::{self, ClockCache, KeyHint};
use quickstream::ui::playlists::{NamePurpose, PendingAdd, PlaylistModal};
use quickstream::ui::screen::{Screen, WidthTier};
use quickstream::ui::theme::{self, ColorDepth, Palette};
use quickstream::ui::AppUI;

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
    // Bold INK, the one style for titles, names and numbers that stand out.
    assert_eq!(theme::bold(), Style::new().fg(p.ink).add_modifier(Modifier::BOLD));
    assert_eq!(theme::label(true), theme::bold());
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

/// `kit::fit_top_hints`: the rightmost top hint gives way while the top hints are too wide,
/// then the next priority takes their place; only hints that fit whole are kept, none when
/// even one doesn't.
#[test]
fn fit_top_hints_keeps_only_whole_hints() {
    common::isolate();
    let keys = |hints: &[kit::KeyHint]| hints.iter().map(|h| h.keys.clone()).collect::<Vec<_>>();
    let hints = now_playing_hints();
    assert_eq!(
        kit::fit_top_hints(&hints, 21),
        hints,
        "room for both top hints: nothing goes"
    );
    // `? KEYS` (rightmost) gives way; `N QUEUE` (9 columns) stays while it fits.
    let fitted = kit::fit_top_hints(&hints, 20);
    assert!(!keys(&fitted).contains(&"?".to_string()), "{fitted:?}");
    assert!(keys(&fitted).contains(&"N".to_string()), "{fitted:?}");
    assert_eq!(kit::top_priority_width(&kit::fit_top_hints(&hints, 9)), 9);
    // Narrower than `N QUEUE`: the next priority's hints that fit whole, then none.
    for width in 0..9 {
        let fitted = kit::fit_top_hints(&hints, width);
        assert!(kit::top_priority_width(&fitted) <= width, "{width}: {fitted:?}");
        let line = kit::key_row(&fitted, width as u16, None);
        assert!(line_width(&line) <= width, "{width}: {:?}", line_text(&line));
    }
    assert!(kit::fit_top_hints(&hints, 0).is_empty());
    assert!(kit::fit_top_hints(&[hint("ENTER", "SAVE", 1)], 9).is_empty());
}

/// `kit::whole_key_row`: a modal footer's row of whole keys only. Where `key_row` keeps its top
/// hints even when they are cut at the edge, it keeps only those that fit whole.
#[test]
fn whole_key_row_keeps_only_whole_keys() {
    common::isolate();
    let hints = vec![hint("ENTER", "SAVE", 1), hint("ESC", "CANCEL", 1)];
    // ` ENTER ` ` SAVE` (12), 4 columns, ` ESC ` ` CANCEL` (12).
    assert_eq!(
        line_text(&kit::whole_key_row(&hints, 28)).trim_end(),
        " ENTER  SAVE     ESC  CANCEL"
    );
    for width in [27u16, 20, 12] {
        let line = kit::whole_key_row(&hints, width);
        assert_eq!(line_width(&line), usize::from(width));
        assert_eq!(line_text(&line).trim_end(), " ENTER  SAVE", "{width}");
    }
    for width in [11u16, 1, 0] {
        assert_eq!(line_text(&kit::whole_key_row(&hints, width)).trim(), "", "{width}");
    }
}

#[test]
fn top_priority_width_measures_the_hints_never_dropped() {
    common::isolate();
    // ` N ` + ` QUEUE` (9), 4 columns, ` ? ` + ` KEYS` (8).
    assert_eq!(kit::top_priority_width(&now_playing_hints()), 21);
    assert_eq!(kit::top_priority_width(&[hint("?", "KEYS", 1)]), 8);
    assert_eq!(kit::top_priority_width(&[]), 0);
    // It is the width of the narrowest row those hints can make.
    let line = kit::key_row(&now_playing_hints(), 0, None);
    assert_eq!(line_width(&line), 21);
}

#[test]
fn clip_cuts_without_padding() {
    common::isolate();
    assert_eq!(kit::clip("Halcyon", 10), "Halcyon");
    assert_eq!(kit::clip("Halcyon", 7), "Halcyon");
    assert_eq!(kit::clip("Halcyon Drift", 8), "Halcyon…");
    assert_eq!(kit::clip("日本語", 4), "日…");
    assert_eq!(kit::clip("日本語", 3), "日…");
    assert_eq!(kit::clip("abc", 0), "");
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

/// One duration format for every list row, total and the strip: `MM:SS` under an hour,
/// `H:MM:SS` from one (PR #68 review: rows read `01:15:00` while the strip read `75:00`).
#[test]
fn duration_is_mm_ss_under_an_hour_and_h_mm_ss_from_one() {
    common::isolate();
    assert_eq!(kit::duration(0.0), "00:00");
    assert_eq!(kit::duration(-3.0), "00:00");
    assert_eq!(kit::duration(242.0), "04:02");
    assert_eq!(kit::duration(241.6), "04:02");
    assert_eq!(kit::duration(3599.0), "59:59");
    assert_eq!(kit::duration(3599.6), "1:00:00");
    assert_eq!(kit::duration(3600.0), "1:00:00");
    assert_eq!(kit::duration(4500.0), "1:15:00");
    assert_eq!(kit::duration(36_005.0), "10:00:05");
}

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

// ---------------------------------------------------------------------------------------------
// The screen chrome drawn by `AppUI::render`: background, header, tab row, strip and key row.
// ---------------------------------------------------------------------------------------------

/// A fresh app with an empty queue, browsing a temp dir of its own.
fn chrome_app(tag: &str) -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_chrome_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    (AppUI::new(Some(dir.clone()), settings_mgr), dir)
}

fn track(dir: &Path, title: &str, artist: &str) -> AudioMetadata {
    AudioMetadata {
        filepath: dir.join(format!("{title}.flac")),
        title: title.to_string(),
        artist: artist.to_string(),
        duration_seconds: 352.0,
        format_name: "flac".to_string(),
        ..Default::default()
    }
}

/// The key row: the last row holding the `?` keycap.
fn key_row_index(buffer: &ratatui::buffer::Buffer) -> u16 {
    (0..buffer.area.height)
        .rev()
        .find(|&y| tui::row(buffer, y).contains(" ?  KEYS"))
        .unwrap_or_else(|| panic!("no key row with ? KEYS:\n{}", tui::rows(buffer).join("\n")))
}

/// Number of keycaps (runs of cells with the keycap background) in row `y`.
fn keycap_count(buffer: &ratatui::buffer::Buffer, y: u16) -> usize {
    let rule = theme::palette().rule;
    let mut count = 0;
    let mut inside = false;
    for x in 0..buffer.area.width {
        let is_cap = buffer[(x, y)].bg == rule;
        if is_cap && !inside {
            count += 1;
        }
        inside = is_cap;
    }
    count
}

/// The header: the row holding `QUICKSTREAM`.
fn header_row(buffer: &ratatui::buffer::Buffer) -> String {
    let y = tui::find_row(buffer, "QUICKSTREAM")
        .unwrap_or_else(|| panic!("no header row:\n{}", tui::rows(buffer).join("\n")));
    tui::row(buffer, y)
}

/// `HH:MM` at the end of `text` (ignoring trailing spaces).
fn ends_with_clock(text: &str) -> bool {
    let t: Vec<char> = text.trim_end().chars().collect();
    t.len() >= 5
        && t[t.len() - 5..]
            .iter()
            .enumerate()
            .all(|(i, c)| if i == 2 { *c == ':' } else { c.is_ascii_digit() })
}

#[test]
fn every_cell_is_drawn_on_ground_at_120x40() {
    common::isolate();
    let (mut app, dir) = chrome_app("ground");
    let p = theme::palette();
    for tab in 0..5 {
        app.active_tab = tab;
        let buffer = tui::render(&mut app, 120, 40);
        for y in 0..40 {
            for x in 0..120 {
                let bg = buffer[(x, y)].bg;
                // GROUND everywhere, except keycaps (RULE) and the cursor / active tab chip (INK).
                assert!(
                    bg == p.ground || bg == p.rule || bg == p.ink,
                    "tab {tab}: cell ({x}, {y}) has background {bg:?}\n{}",
                    tui::rows(&buffer).join("\n")
                );
            }
        }
        // Margins, header, blank rows, rule and strip have nothing but GROUND.
        for y in [0, 1, 2, 4, 5, 39] {
            assert!(
                (0..120).all(|x| buffer[(x, y)].bg == p.ground),
                "tab {tab}: row {y} is not all GROUND: {:?}",
                tui::row(&buffer, y)
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn header_shows_name_version_output_and_clock() {
    common::isolate();
    let (mut app, dir) = chrome_app("header");
    let buffer = tui::render(&mut app, 120, 40);
    let header = header_row(&buffer);
    let name = format!("  QUICKSTREAM ▪ {}", env!("CARGO_PKG_VERSION"));
    assert!(header.starts_with(&name), "{header:?}");
    assert!(header.contains("LOCAL/MPV"), "{header:?}");
    assert!(!header.contains("WEB :"), "no web server, no WEB badge: {header:?}");
    assert!(ends_with_clock(&header), "clock HH:MM at the right: {header:?}");
    assert!(header.ends_with("  "), "2-column gutter on the right: {header:?}");

    let styles = tui::text_styles(&buffer, "QUICKSTREAM").unwrap();
    assert!(styles.iter().all(|s| s.fg == Some(theme::palette().ink)));
    assert!(styles.iter().all(|s| s.add_modifier.contains(Modifier::BOLD)));
    let styles = tui::text_styles(&buffer, env!("CARGO_PKG_VERSION")).unwrap();
    assert!(styles.iter().all(|s| s.fg == Some(theme::palette().dim)));
    let styles = tui::text_styles(&buffer, "LOCAL/MPV").unwrap();
    assert!(styles.iter().all(|s| s.fg == Some(theme::palette().mid)));

    // The header names the backend the player runs, not the setting: mpv is set, but without
    // mpv the local player falls back to ffplay.
    app.settings_mgr.settings.player_backend = "mpv".to_string();
    app.player = Box::new(FakePlayer {
        backend: Some("ffplay"),
        ..Default::default()
    });
    let header = header_row(&tui::render(&mut app, 120, 40));
    assert!(header.contains("LOCAL/FFPLAY"), "{header:?}");

    // A player that names no backend (before the worker's first report) falls back to the setting.
    app.player = Box::new(FakePlayer::default());
    app.settings_mgr.settings.player_backend = "ffplay".to_string();
    let header = header_row(&tui::render(&mut app, 120, 40));
    assert!(header.contains("LOCAL/FFPLAY"), "{header:?}");
    app.settings_mgr.settings.player_backend = "mpv".to_string();
    let header = header_row(&tui::render(&mut app, 120, 40));
    assert!(header.contains("LOCAL/MPV"), "{header:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn header_shows_web_port_only_while_the_server_runs() {
    common::isolate();
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = true;
    settings_mgr.settings.web_server_port = 0;
    let mut app = AppUI::new(None, settings_mgr);
    let port = app.web_server.as_ref().expect("server running").port;

    let header = header_row(&tui::render(&mut app, 120, 40));
    assert!(header.contains(&format!("WEB :{port}")), "{header:?}");
    assert!(ends_with_clock(&header), "{header:?}");

    // Narrow: the WEB badge goes first, the clock stays.
    let header = header_row(&tui::render(&mut app, 40, 30));
    assert!(!header.contains("WEB :"), "{header:?}");
    assert!(ends_with_clock(&header), "{header:?}");

    app.web_server = None;
    let header = header_row(&tui::render(&mut app, 120, 40));
    assert!(!header.contains("WEB :"), "{header:?}");
}

#[test]
fn tall_layout_rows_at_120x40_and_120x29() {
    common::isolate();
    let (mut app, dir) = chrome_app("rows");
    let width = 120usize;
    let rule_row = format!("  {}  ", "─".repeat(width - 4));

    app.active_tab = 1;
    let buffer = tui::render(&mut app, 120, 40);
    let rows = tui::rows(&buffer);
    assert!(rows[0].trim().is_empty(), "top margin: {:?}", rows[0]);
    assert!(rows[1].contains("QUICKSTREAM"));
    assert!(rows[2].trim().is_empty());
    assert!(rows[3].contains("2 QUEUE & LIBRARY"), "tab row: {:?}", rows[3]);
    assert!(
        rows[4].trim().is_empty() && rows[5].trim().is_empty(),
        "2-row gap at 40 rows"
    );
    assert_eq!(rows[35], rule_row);
    assert!(rows[36].contains("NO TRACK LOADED"), "strip: {:?}", rows[36]);
    assert!(rows[37].trim().is_empty());
    assert_eq!(key_row_index(&buffer), 38);
    assert!(rows[39].trim().is_empty(), "bottom margin");
    assert!(
        (0..120).all(|x| buffer[(x, 35)].fg == theme::palette().rule || buffer[(x, 35)].symbol() == " "),
        "rule in RULE"
    );

    // Under 30 rows the gap under the tab row is one row.
    let rows = tui::rows(&tui::render(&mut app, 120, 29));
    assert!(rows[3].contains("2 QUEUE & LIBRARY"));
    assert!(rows[4].trim().is_empty());
    assert!(!rows[5].trim().is_empty(), "content starts at row 5: {:?}", rows[5]);

    // Now Playing has no rule and no strip.
    app.active_tab = 0;
    let buffer = tui::render(&mut app, 120, 40);
    let rows = tui::rows(&buffer);
    assert_eq!(key_row_index(&buffer), 38);
    assert!(!rows.contains(&rule_row), "no rule on Now Playing");
    // The tab's own empty state says NO TRACK LOADED too; the strip's starts with its glyph.
    assert!(
        !rows.iter().any(|r| r.contains("■ NO TRACK LOADED")),
        "no strip on Now Playing"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn short_terminal_folds_the_header_into_the_tab_row() {
    common::isolate();
    let (mut app, dir) = chrome_app("short");
    app.active_tab = 1;
    let buffer = tui::render(&mut app, 100, 20);
    let rows = tui::rows(&buffer);
    assert!(
        !rows.iter().any(|r| r.contains("QUICKSTREAM")),
        "no header row under 24 rows"
    );
    assert!(rows[0].contains("2 QUEUE"), "tab row first: {:?}", rows[0]);
    assert!(ends_with_clock(&rows[0]), "clock on the tab row: {:?}", rows[0]);
    assert!(
        rows[18].contains("NO TRACK LOADED"),
        "strip above the key row: {:?}",
        rows[18]
    );
    assert_eq!(key_row_index(&buffer), 19);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn strip_shows_on_tabs_2_to_5_but_not_on_now_playing() {
    common::isolate();
    let (mut app, dir) = chrome_app("strip");
    app.queue.add_track(track(&dir, "Halcyon Drift", "Mira Lenz"));
    for tab in 0..5 {
        app.active_tab = tab;
        let buffer = tui::render(&mut app, 120, 40);
        let strip = tui::find_row(&buffer, "■ HALCYON DRIFT");
        if tab == 0 {
            assert!(strip.is_none(), "Now Playing has no strip");
            continue;
        }
        let y = strip.unwrap_or_else(|| panic!("tab {tab}: no strip\n{}", tui::rows(&buffer).join("\n")));
        let row = tui::row(&buffer, y);
        assert!(row.contains("MIRA LENZ"), "artist at 120 columns: {row:?}");
        assert!(row.contains("00:00 ▕"), "elapsed time and bar: {row:?}");
        assert!(row.contains("▏ 05:52"), "duration after the bar: {row:?}");
        assert!(row.contains("VOL "), "volume: {row:?}");
        let title = tui::text_styles(&buffer, "HALCYON DRIFT").unwrap();
        assert!(title.iter().all(|s| s.add_modifier.contains(Modifier::BOLD)));
    }

    // Under 90 columns the strip shows the title only.
    app.active_tab = 1;
    let buffer = tui::render(&mut app, 85, 30);
    let y = tui::find_row(&buffer, "■ HALCYON DRIFT").expect("strip at 85 columns");
    assert!(!tui::row(&buffer, y).contains("MIRA LENZ"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn strip_marks_a_pending_skip_as_queued() {
    common::isolate();
    let (mut app, dir) = chrome_app("strip_skip");
    app.queue.add_track(track(&dir, "One", "A"));
    app.queue.add_track(track(&dir, "Two", "B"));
    app.queue.set_current_index(1);
    app.pending_track_skip = true;
    app.active_tab = 1;
    let buffer = tui::render(&mut app, 120, 40);
    // The strip is the last row naming the track (tab 2's queue rows name it too).
    let y = (0..buffer.area.height)
        .rev()
        .find(|&y| tui::row(&buffer, y).contains("TWO"))
        .expect("strip names the skip target");
    let row = tui::row(&buffer, y);
    assert!(row.contains("QUEUED"), "{row:?}");
    assert!(row.contains("00:00"), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn key_row_trims_with_width_and_always_keeps_help() {
    common::isolate();
    let (mut app, dir) = chrome_app("keys");
    app.status_message.clear();
    for tab in 0..5 {
        app.active_tab = tab;
        for width in [120u16, 100, 85, 80, 70, 40] {
            let buffer = tui::render(&mut app, width, 30);
            let y = key_row_index(&buffer);
            let row = tui::row(&buffer, y);
            assert!(row.starts_with("   "), "2-column gutter, then a keycap: {row:?}");
            if tab == 0 {
                assert!(row.contains(" N  QUEUE"), "tab {tab} at {width}: {row:?}");
                assert!(!row.to_uppercase().contains("ESC"), "tab {tab} at {width}: {row:?}");
            }
            if (80..90).contains(&width) {
                assert!(keycap_count(&buffer, y) <= 5, "at most 5 keys at {width}: {row:?}");
            }
        }
    }

    // Wide enough for the whole Queue & Library row.
    app.active_tab = 1;
    app.queue_focus = false;
    let buffer = tui::render(&mut app, 120, 30);
    let row = tui::row(&buffer, key_row_index(&buffer));
    for label in [
        " H L  PANE",
        " ENTER  OPEN / ADD",
        " A  ADD ALL",
        " /  SPOTLIGHT",
        " F  SEARCH",
        " V  TAGS/FILES",
    ] {
        assert!(row.contains(label), "{label:?} missing: {row:?}");
    }
    // Narrower rows drop the least important keys first.
    let buffer = tui::render(&mut app, 70, 30);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(!row.contains("TAGS/FILES"), "{row:?}");
    assert!(row.contains("ENTER"), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn now_playing_key_row_shows_play_state_seek_step_and_volume() {
    common::isolate();
    let (mut app, dir) = chrome_app("np_keys");
    app.active_tab = 0;
    app.settings_mgr.settings.seek_step_seconds = 30.0;
    let buffer = tui::render(&mut app, 120, 40);
    let row = tui::row(&buffer, key_row_index(&buffer));
    for label in [
        " SPC  PLAY",
        " ←→  TRACK",
        " [ ]  SEEK 30S",
        " +−  VOL",
        " N  QUEUE",
        " ?  KEYS",
    ] {
        assert!(row.contains(label), "{label:?} missing: {row:?}");
    }
    assert!(row.contains("VOL ▮") || row.contains("VOL ▯"), "volume meter: {row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn playlists_key_row_follows_the_focused_pane() {
    common::isolate();
    let (mut app, dir) = chrome_app("pl_keys");
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The tracks pane only takes focus with a playlist selected.
    if app.playlists.store.index_of("Chrome Keys").is_none() {
        app.playlists.store.create("Chrome Keys").unwrap();
    }
    app.active_tab = 4;
    app.playlists.tracks_focus = false;
    let buffer = tui::render(&mut app, 120, 40);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(row.contains(" C ⇧R ⇧D  NEW / RENAME / DELETE"), "{row:?}");
    assert!(!row.contains("MOVE TRACK"), "{row:?}");

    app.playlists.tracks_focus = true;
    let buffer = tui::render(&mut app, 120, 40);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(row.contains(" ⇧J ⇧K  MOVE TRACK"), "{row:?}");
    assert!(row.contains(" D  REMOVE"), "{row:?}");
    assert!(row.contains(" ENTER  PLAY"), "{row:?}");
    assert!(!row.contains("RENAME"), "c/R/D do nothing in the tracks pane: {row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn status_message_sits_at_the_right_of_the_key_row() {
    common::isolate();
    let (mut app, dir) = chrome_app("status");
    app.active_tab = 2;
    let p = theme::palette();

    app.status_message = "Scanning for DLNA MediaRenderers...".to_string();
    let buffer = tui::render(&mut app, 120, 40);
    let y = key_row_index(&buffer);
    let row = tui::row(&buffer, y);
    assert!(
        row.trim_end().ends_with("Scanning for DLNA MediaRenderers..."),
        "{row:?}"
    );
    let styles = tui::text_styles(&buffer, "Scanning for DLNA").unwrap();
    assert!(styles.iter().all(|s| s.fg == Some(p.mid)), "plain status in MID");

    for message in [
        "Playback error: broken pipe",
        "Failed to start server: in use",
        "Couldn't read Music",
    ] {
        app.status_message = message.to_string();
        let buffer = tui::render(&mut app, 120, 40);
        let styles = tui::text_styles(&buffer, message).unwrap_or_else(|| panic!("{message:?} not shown"));
        assert!(styles.iter().all(|s| s.fg == Some(p.error)), "{message:?} in ERROR");
    }

    // Redundant messages are not repeated.
    app.status_message = "Welcome to Quickstream (Rust)".to_string();
    let buffer = tui::render(&mut app, 120, 40);
    assert!(tui::find_row(&buffer, "Welcome").is_none());

    // A long message takes the room of the least important keys, never `? KEYS`.
    app.status_message = "x".repeat(150);
    let buffer = tui::render(&mut app, 120, 40);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(row.contains(" ?  KEYS") && row.contains('…'), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn now_playing_status_shares_the_row_with_the_volume_meter() {
    common::isolate();
    let (mut app, dir) = chrome_app("np_status");
    app.active_tab = 0;
    app.status_message = "Volume 65%".to_string();
    let buffer = tui::render(&mut app, 120, 40);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(row.contains("Volume 65%"), "{row:?}");
    assert!(row.contains("VOL ▮") || row.contains("VOL ▯"), "{row:?}");
    assert!(row.find("Volume 65%") < row.find("VOL ▮").or(row.find("VOL ▯")));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn chrome_renders_without_panicking_down_to_40x12() {
    common::isolate();
    let (mut app, dir) = chrome_app("sizes");
    app.queue.add_track(track(&dir, "Halcyon Drift", "Mira Lenz"));
    app.status_message = "Couldn't read a folder with a rather long name".to_string();
    for tab in 0..5 {
        app.active_tab = tab;
        for (w, h) in [
            (120, 40),
            (100, 30),
            (85, 30),
            (80, 24),
            (70, 24),
            (80, 20),
            (40, 12),
            (12, 4),
            (1, 1),
        ] {
            let buffer = tui::render(&mut app, w, h);
            if w >= 40 {
                let row = tui::row(&buffer, key_row_index(&buffer));
                assert!(row.contains(" ?  KEYS"), "tab {tab} at {w}x{h}: {row:?}");
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn spotlight_hint_is_cleared_when_spotlight_closes() {
    common::isolate();
    let (mut app, dir) = chrome_app("spotlight_hint");
    app.active_tab = 1;
    app.open_spotlight();
    assert!(app.status_message.starts_with("Spotlight:"), "{:?}", app.status_message);
    app.close_spotlight();
    assert!(
        !app.status_message.starts_with("Spotlight:"),
        "the hint goes with the spotlight: {:?}",
        app.status_message
    );

    // It no longer hides keys on other tabs.
    app.active_tab = 2;
    let buffer = tui::render(&mut app, 100, 30);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(!row.contains("Spotlight"), "{row:?}");
    assert!(row.contains(" R  RESCAN"), "{row:?}");

    // A status set while the spotlight was open stays.
    app.active_tab = 1;
    app.open_spotlight();
    app.status_message = "Added to queue: Halcyon Drift".to_string();
    app.close_spotlight();
    assert_eq!(app.status_message, "Added to queue: Halcyon Drift");

    // Leaving the folder closes the spotlight and its hint too.
    app.open_spotlight();
    app.navigate_browser(dir.clone());
    assert!(app.browser_spotlight.is_none());
    assert!(
        !app.status_message.starts_with("Spotlight:"),
        "{:?}",
        app.status_message
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The tab row: the first row from the top that holds `label` (the active tab's label, which the
/// header above it never holds).
fn tab_row_index(buffer: &ratatui::buffer::Buffer, label: &str) -> u16 {
    tui::find_row(buffer, label)
        .unwrap_or_else(|| panic!("no tab row with {label:?}:\n{}", tui::rows(buffer).join("\n")))
}

/// The active tab chip on tab row `y`: its text and first column.
fn active_chip(buffer: &ratatui::buffer::Buffer, y: u16) -> (String, u16) {
    let ink = theme::palette().ink;
    let xs: Vec<u16> = (0..buffer.area.width).filter(|&x| buffer[(x, y)].bg == ink).collect();
    assert!(
        !xs.is_empty(),
        "no chip on the tab row:\n{}",
        tui::rows(buffer).join("\n")
    );
    let text = xs
        .iter()
        .map(|&x| buffer[(x, y)].symbol().to_string())
        .collect::<String>();
    (text, xs[0])
}

#[test]
fn tab_row_keeps_the_active_chip_whole_down_to_20_columns() {
    common::isolate();
    let (mut app, dir) = chrome_app("tabs_narrow");
    let labels = ["1 NOW", "2 QUEUE", "3 DEVICES", "4 SETTINGS", "5 PLAYLISTS"];
    for width in [36u16, 30, 24, 20] {
        for (tab, label) in labels.iter().enumerate() {
            app.active_tab = tab;
            let buffer = tui::render(&mut app, width, 24);
            let y = tab_row_index(&buffer, label);
            let row = tui::row(&buffer, y);
            let (chip, _) = active_chip(&buffer, y);
            assert_eq!(chip, format!(" {label} "), "tab {tab} at {width}: {row:?}");
            assert!(row.ends_with("  "), "right gutter kept at {width}: {row:?}");
            // The numbers that remain are the ones nearest the active tab, in order.
            let numbers: Vec<usize> = row.split_whitespace().filter_map(|w| w.parse::<usize>().ok()).collect();
            let mut sorted = numbers.clone();
            sorted.sort();
            assert_eq!(numbers, sorted, "tab order kept: {row:?}");
            let farthest = numbers.iter().map(|&n| (n as isize - 1 - tab as isize).abs()).max();
            let missing = (1..=5)
                .filter(|n| !numbers.contains(n) && *n != tab + 1)
                .map(|n| (n as isize - 1 - tab as isize).abs())
                .min();
            if let (Some(far), Some(miss)) = (farthest, missing) {
                assert!(far <= miss, "a farther tab kept over a nearer one: {row:?}");
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tab_row_cuts_the_active_chip_with_an_ellipsis_when_it_alone_does_not_fit() {
    common::isolate();
    let (mut app, dir) = chrome_app("tabs_tiny");
    app.active_tab = 4;
    let buffer = tui::render(&mut app, 14, 24);
    let y = tab_row_index(&buffer, "5 PLAYL…");
    let (chip, x) = active_chip(&buffer, y);
    assert_eq!(chip, " 5 PLAYL… ", "{:?}", tui::row(&buffer, y));
    assert_eq!(x, 2, "after the left gutter");
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Modals over the chrome
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Modal {
    Help,
    MixPicker,
    Search,
    NamePrompt,
    ConfirmDelete,
    AddPicker,
}

const MODALS: [Modal; 6] = [
    Modal::Help,
    Modal::MixPicker,
    Modal::Search,
    Modal::NamePrompt,
    Modal::ConfirmDelete,
    Modal::AddPicker,
];

fn open_modal(app: &mut AppUI, modal: Modal) {
    match modal {
        Modal::Help => app.show_help_modal = true,
        Modal::MixPicker => app.show_mix_picker_modal = true,
        Modal::Search => app.open_fuzzy_search(),
        Modal::NamePrompt => {
            app.playlists.modal = Some(PlaylistModal::NamePrompt {
                purpose: NamePurpose::Create,
                input: "Road".to_string(),
                error: None,
            })
        }
        Modal::ConfirmDelete => {
            app.playlists.modal = Some(PlaylistModal::ConfirmDelete {
                name: "Road Trip".to_string(),
                count: 3,
            })
        }
        Modal::AddPicker => {
            app.playlists.modal = Some(PlaylistModal::AddPicker {
                pending: PendingAdd::Entries {
                    label: "Halcyon Drift".to_string(),
                    entries: Vec::new(),
                },
                cursor: 0,
            })
        }
    }
}

fn close_modals(app: &mut AppUI) {
    app.show_help_modal = false;
    app.show_mix_picker_modal = false;
    if let Some(mut search) = app.search_modal.take() {
        search.close();
    }
    app.playlists.modal = None;
}

#[test]
fn modals_sit_on_ground_not_the_terminal_default() {
    common::isolate();
    let (mut app, dir) = chrome_app("modal_ground");
    let p = theme::palette();
    for modal in MODALS {
        open_modal(&mut app, modal);
        let buffer = tui::render(&mut app, 120, 40);
        let screen = tui::rows(&buffer).join("\n");
        for y in 0..40 {
            for x in 0..120 {
                // The cell under the second half of a wide character (the modal titles' emoji) is
                // never drawn; ratatui resets it.
                if x > 0 && Span::raw(buffer[(x - 1, y)].symbol()).width() == 2 {
                    continue;
                }
                let bg = buffer[(x, y)].bg;
                // GROUND, keycaps (RULE), cursor rows (INK) and the delete confirmation's ERROR
                // chip and `Y` keycap, never the terminal default or black.
                assert!(
                    [p.ground, p.rule, p.ink, p.error].contains(&bg),
                    "{modal:?}: cell ({x}, {y}) has background {bg:?}\n{screen}"
                );
            }
        }
        close_modals(&mut app);
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The keys each modal's handler acts on, as the key row shows them.
fn modal_keys(modal: Modal) -> &'static [&'static str] {
    match modal {
        Modal::Help => &[" ? ESC  CLOSE"],
        Modal::MixPicker => &[" J K  SELECT", " ENTER  START MIX", " 1-5  PROFILE", " ESC  CANCEL"],
        Modal::Search => &[
            " ↑↓  SELECT",
            " ENTER  ADD & CLOSE",
            " TAB  ADD TO QUEUE",
            " ⇧TAB  ADD ALL",
            " ESC  CANCEL",
        ],
        Modal::NamePrompt => &[" ENTER  SAVE", " ESC  CANCEL"],
        Modal::ConfirmDelete => &[" Y  DELETE", " ANY KEY  CANCEL"],
        Modal::AddPicker => &[" J K  SELECT", " ENTER  ADD", " ESC  CANCEL"],
    }
}

#[test]
fn key_row_shows_the_open_modals_keys_instead_of_the_tabs() {
    common::isolate();
    let (mut app, dir) = chrome_app("modal_keys");
    app.status_message.clear();
    app.active_tab = 1;
    app.queue_focus = false;
    // Where the tab's own key row sits with no modal open.
    let tab_key_row = key_row_index(&tui::render(&mut app, 120, 40));
    for modal in MODALS {
        open_modal(&mut app, modal);
        app.status_message.clear();
        let buffer = tui::render(&mut app, 120, 40);
        let keys = modal_keys(modal);
        let y = (0..40)
            .rev()
            .find(|&y| tui::row(&buffer, y).contains(keys[0]))
            .unwrap_or_else(|| panic!("{modal:?}: no key row\n{}", tui::rows(&buffer).join("\n")));
        assert_eq!(y, tab_key_row, "{modal:?}: the key row stays in place");
        let row = tui::row(&buffer, y);
        for key in keys {
            assert!(row.contains(key), "{modal:?}: {key:?} missing: {row:?}");
        }
        // Nothing from tab 2, and no `? KEYS`: no modal opens help with `?` (in the name
        // prompt and the search query it is typed text).
        assert!(
            !row.contains("SPOTLIGHT") && !row.contains("PANE"),
            "{modal:?}: {row:?}"
        );
        assert!(!row.contains(" ?  KEYS"), "{modal:?}: {row:?}");
        close_modals(&mut app);
    }

    // Closed again, the tab's keys come back.
    let buffer = tui::render(&mut app, 120, 40);
    let row = tui::row(&buffer, key_row_index(&buffer));
    assert!(row.contains(" /  SPOTLIGHT"), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}

/// The delete confirmation's footer drops `ANY KEY CANCEL` when the modal is too narrow for it,
/// so on a narrow screen the key row must keep both keys: a status message is cut instead of
/// `ANY KEY CANCEL`, which would then show nowhere.
#[test]
fn narrow_delete_confirmation_keeps_any_key_cancel_beside_a_status() {
    common::isolate();
    let (mut app, dir) = chrome_app("delete_keys_narrow");
    app.active_tab = 4;
    open_modal(&mut app, Modal::ConfirmDelete);
    for status in [
        "Nothing is playing",
        "Couldn't rename \"Old\" to \"New\": permission denied",
    ] {
        app.status_message = status.to_string();
        for width in [40, 44, 50] {
            let buffer = tui::render(&mut app, width, 24);
            let rows = tui::rows(&buffer).join("\n");
            let y = (0..24)
                .rev()
                .find(|&y| tui::row(&buffer, y).contains(" Y  DELETE"))
                .unwrap_or_else(|| panic!("{width}: no key row\n{rows}"));
            let row = tui::row(&buffer, y);
            assert!(
                row.contains(" ANY KEY  CANCEL"),
                "{width} cols, status {status:?}: ANY KEY CANCEL dropped from the key row: {row:?}\n{rows}"
            );
            // The modal's footer shows it whole or not at all, never cut.
            let footer_y = (0..y)
                .find(|&y| tui::row(&buffer, y).contains(" Y  DELETE"))
                .unwrap_or_else(|| panic!("{width}: no modal footer\n{rows}"));
            let footer = tui::row(&buffer, footer_y);
            assert!(
                !footer.contains(" ANY") || footer.contains(" ANY KEY  CANCEL"),
                "{width} cols: footer cuts ANY KEY CANCEL: {footer:?}\n{rows}"
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The keys in row `y` between columns `left` and `right` as (keycap, label) pairs: each run
/// of keycap cells (RULE background) and the text after it, up to the next keycap.
fn key_pairs(buffer: &ratatui::buffer::Buffer, y: u16, left: u16, right: u16) -> Vec<(String, String)> {
    let rule = theme::palette().rule;
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut in_keycap = false;
    for x in left..right {
        let cell = &buffer[(x, y)];
        let is_keycap = cell.bg == rule;
        if is_keycap && !in_keycap {
            pairs.push((String::new(), String::new()));
        }
        in_keycap = is_keycap;
        if let Some((key, label)) = pairs.last_mut() {
            if is_keycap {
                key.push_str(cell.symbol());
            } else {
                label.push_str(cell.symbol());
            }
        }
    }
    pairs
        .into_iter()
        .map(|(k, l)| {
            // A label ends where a wide gap starts (the status or the meter may follow).
            let label = l.trim().split("   ").next().unwrap_or("").trim().to_string();
            (k.trim().to_string(), label)
        })
        .collect()
}

/// A modal's footer and the key row name its keys in the same words: every key the footer
/// shows is in the key row with the footer's label (the key row may add keys the footer leaves
/// out, such as `↑↓ SELECT` in search).
#[test]
fn modal_footers_and_the_key_row_name_keys_alike() {
    common::isolate();
    let (mut app, dir) = chrome_app("modal_footer_agree");
    app.active_tab = 1;
    for modal in MODALS {
        open_modal(&mut app, modal);
        app.status_message.clear();
        let buffer = tui::render(&mut app, 120, 40);
        let rows = tui::rows(&buffer).join("\n");
        // The modal's frame: its bottom border, and the footer the last row inside with keycaps.
        let (left, top) = tui::find_text(&buffer, "┌").unwrap_or_else(|| panic!("{modal:?}\n{rows}"));
        let bottom = (top + 1..40).find(|&y| buffer[(left, y)].symbol() == "└").unwrap();
        let right = (left + 1..120).find(|&x| buffer[(x, top)].symbol() == "┐").unwrap();
        let footer_y = (top + 1..bottom)
            .rev()
            .find(|&y| !key_pairs(&buffer, y, left + 1, right).is_empty())
            .unwrap_or_else(|| panic!("{modal:?}: no footer\n{rows}"));
        let footer = key_pairs(&buffer, footer_y, left + 1, right);
        let key_row_y = (bottom + 1..40)
            .find(|&y| !key_pairs(&buffer, y, 0, 120).is_empty())
            .unwrap_or_else(|| panic!("{modal:?}: no key row under the modal\n{rows}"));
        let key_row = key_pairs(&buffer, key_row_y, 0, 120);
        for pair in &footer {
            assert!(
                key_row.contains(pair),
                "{modal:?}: footer {pair:?} not in the key row {key_row:?}\n{rows}"
            );
        }
        close_modals(&mut app);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn strip_names_the_mix_profile_while_autopilot_runs_when_there_is_room() {
    common::isolate();
    let (mut app, dir) = chrome_app("strip_profile");
    app.queue.add_track(track(&dir, "Halcyon Drift", "Mira Lenz"));
    app.active_mix_profile = quickstream::analysis::MixProfile::BpmFlow {
        target_bpm: 120.0,
        tolerance: 0.15,
    };
    app.active_tab = 1;
    let strip = |app: &mut AppUI, width: u16| {
        let buffer = tui::render(app, width, 30);
        // The last row naming the track: tab 2's queue rows name it too.
        let y = (0..buffer.area.height)
            .rev()
            .find(|&y| tui::row(&buffer, y).contains("HALCYON"))
            .unwrap_or_else(|| panic!("no strip at {width}"));
        tui::row(&buffer, y)
    };

    // Autopilot off: no profile.
    assert!(!strip(&mut app, 120).contains("BPM FLOW"));

    app.autopilot_dir = Some(dir.clone());
    for tab in 1..5 {
        app.active_tab = tab;
        let row = strip(&mut app, 120);
        assert!(row.contains("BPM FLOW"), "tab {tab}: {row:?}");
        let styles = tui::text_styles(&tui::render(&mut app, 120, 30), "BPM FLOW").unwrap();
        assert!(styles.iter().all(|s| s.fg == Some(theme::palette().mid)), "in MID");
        // Everything else is still whole.
        for part in ["HALCYON DRIFT", "MIRA LENZ", "00:00 ▕", "▏ 05:52", "VOL "] {
            assert!(row.contains(part), "tab {tab}: {part:?} missing: {row:?}");
        }
    }

    // Lowest priority: dropped first when the strip is short of room.
    app.active_tab = 1;
    let row = strip(&mut app, 85);
    assert!(!row.contains("BPM FLOW"), "{row:?}");
    assert!(row.contains("HALCYON DRIFT"), "title kept whole: {row:?}");
    let row = strip(&mut app, 60);
    assert!(!row.contains("BPM FLOW"), "{row:?}");
    let _ = fs::remove_dir_all(&dir);
}
