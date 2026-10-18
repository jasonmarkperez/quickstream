//! Telemetry panel building blocks in `ui::kit`: the panel header (`panel`), the scroll window of a
//! list (`visible_window`) and list rows laid out in columns (`list_row`).

mod common;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListState};
use ratatui::{Frame, Terminal};

use common::tui;
use quickstream::ui::kit::{self, Col, PanelSpec, RowMark};
use quickstream::ui::theme::Palette;

const P: Palette = Palette::TRUECOLOR;

/// Draw `draw` into a `width` x `height` test terminal and return the buffer.
fn draw(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

fn spec<'a>(label: &'a str, focused: bool) -> PanelSpec<'a> {
    PanelSpec {
        label,
        right: None,
        focused,
        footnote: None,
        compact: false,
    }
}

/// Draw a panel into `area` of a `width` x `height` terminal; returns the buffer and the content
/// area `panel` handed back.
fn draw_panel(width: u16, height: u16, area: Rect, spec: PanelSpec<'static>) -> (Buffer, Rect) {
    let mut content = Rect::default();
    let buf = draw(width, height, |f| content = kit::panel(f, area, spec));
    (buf, content)
}

// ---------------------------------------------------------------------------------------------
// panel
// ---------------------------------------------------------------------------------------------

#[test]
fn focused_panel_draws_label_rule_footnote_blank_then_content() {
    common::isolate();
    let area = Rect::new(2, 1, 40, 12);
    let (buf, content) = draw_panel(
        50,
        14,
        area,
        PanelSpec {
            right: Some("tags"),
            footnote: Some(Line::from("~/Music/Mira Lenz  ·  10 / 14")),
            ..spec("Library", true)
        },
    );

    // Label row: UPPERCASE label at the left, right note ending 2 columns before the edge.
    assert_eq!(tui::find_text(&buf, "LIBRARY"), Some((2, 1)));
    assert_eq!(tui::find_text(&buf, "TAGS"), Some((2 + 40 - 2 - 4, 1)));
    for x in 2..9 {
        let style = tui::cell_style(&buf, x, 1);
        assert_eq!(style.fg, Some(P.ink), "focused label is INK");
        assert!(style.add_modifier.contains(Modifier::BOLD), "focused label is bold");
    }
    assert_eq!(tui::cell_style(&buf, 36, 1).fg, Some(P.dim), "right note is DIM");

    // Rule row: `─` across all but the last 2 columns, then ` ▪`, all INK when focused.
    let rule = tui::row(&buf, 2);
    let expected = format!("{}{} ▪", "  ", "─".repeat(38));
    assert!(rule.starts_with(&expected), "rule row: {rule:?}");
    for x in 2..40 {
        assert_eq!(tui::cell_style(&buf, x, 2).fg, Some(P.ink));
    }
    assert_eq!(tui::cell_style(&buf, 41, 2).fg, Some(P.ink), "focused ▪ is INK");

    // Footnote row in DIM, original case.
    assert_eq!(tui::find_text(&buf, "~/Music/Mira Lenz"), Some((2, 3)));
    assert_eq!(tui::cell_style(&buf, 2, 3).fg, Some(P.dim));

    // Blank row, then content (the text area stops 2 columns short, under the `▪` column).
    assert_eq!(tui::row(&buf, 4).trim(), "");
    assert_eq!(content, Rect::new(2, 5, 38, 8));
}

#[test]
fn unfocused_panel_uses_mid_label_rule_coloured_rule_and_dim_tick() {
    common::isolate();
    let area = Rect::new(0, 0, 30, 8);
    let (buf, _) = draw_panel(30, 8, area, spec("queue", false));

    assert_eq!(tui::find_text(&buf, "QUEUE"), Some((0, 0)));
    let label = tui::cell_style(&buf, 0, 0);
    assert_eq!(label.fg, Some(P.mid), "unfocused label is MID");
    assert!(
        !label.add_modifier.contains(Modifier::BOLD),
        "unfocused label is not bold"
    );
    for x in 0..28 {
        assert_eq!(buf[(x, 1)].symbol(), "─", "column {x}");
        assert_eq!(tui::cell_style(&buf, x, 1).fg, Some(P.rule), "unfocused rule is RULE");
    }
    assert_eq!(tui::cell_style(&buf, 29, 1).fg, Some(P.dim), "unfocused ▪ is DIM");
    assert_eq!(buf[(29, 1)].symbol(), "▪");
}

#[test]
fn panel_without_footnote_starts_content_after_one_blank_row() {
    common::isolate();
    let area = Rect::new(0, 0, 30, 8);
    let (buf, content) = draw_panel(30, 8, area, spec("Devices", true));
    assert_eq!(tui::row(&buf, 2).trim(), "");
    assert_eq!(content, Rect::new(0, 3, 28, 5));
}

#[test]
fn compact_panel_moves_footnote_to_the_end_of_the_label_row() {
    common::isolate();
    let area = Rect::new(2, 0, 37, 10);
    let (buf, content) = draw_panel(
        41,
        10,
        area,
        PanelSpec {
            right: Some("tags"),
            footnote: Some(Line::from("10/14")),
            compact: true,
            ..spec("Library", true)
        },
    );
    // `10/14` ends 2 columns before the panel's right edge, like the right note would.
    assert_eq!(tui::find_text(&buf, "10/14"), Some((2 + 37 - 2 - 5, 0)));
    assert_eq!(tui::cell_style(&buf, 32, 0).fg, Some(P.dim));
    assert!(
        tui::find_text(&buf, "TAGS").is_none(),
        "the footnote takes the right note's place"
    );
    // No footnote row: blank row, then content.
    assert_eq!(tui::row(&buf, 2).trim(), "");
    assert_eq!(content, Rect::new(2, 3, 35, 7));
}

#[test]
fn footnote_spans_keep_their_own_styles_over_dim() {
    common::isolate();
    let underlined = Style::new().add_modifier(Modifier::UNDERLINED);
    let footnote = Line::from(vec![Span::raw(" / "), Span::styled("low", underlined)]);
    let (buf, _) = draw_panel(
        30,
        8,
        Rect::new(0, 0, 30, 8),
        PanelSpec {
            footnote: Some(footnote),
            ..spec("Library", true)
        },
    );
    let (x, y) = tui::find_text(&buf, "low").unwrap();
    let style = tui::cell_style(&buf, x, y);
    assert_eq!(style.fg, Some(P.dim));
    assert!(style.add_modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn long_label_is_cut_and_the_right_note_dropped_when_both_do_not_fit() {
    common::isolate();
    let (buf, _) = draw_panel(
        20,
        6,
        Rect::new(0, 0, 20, 6),
        PanelSpec {
            right: Some("14 tracks · 01:02:40"),
            ..spec("A very long playlist name", true)
        },
    );
    let label_row = tui::row(&buf, 0);
    assert!(label_row.starts_with("A VERY LONG PLAYL… "), "label row: {label_row:?}");
    assert!(!label_row.contains("TRACKS"));
}

#[test]
fn label_and_right_note_share_the_row_when_the_label_can_be_cut_a_little() {
    common::isolate();
    let (buf, _) = draw_panel(
        30,
        6,
        Rect::new(0, 0, 30, 6),
        PanelSpec {
            right: Some("14 tracks"),
            ..spec("Late night driving songs", true)
        },
    );
    let label_row = tui::row(&buf, 0);
    assert_eq!(label_row, format!("{:<28}  ", "LATE NIGHT DRIVI…  14 TRACKS"));
}

#[test]
fn panel_shorter_than_its_header_returns_an_empty_content_area() {
    common::isolate();
    let mut content = Rect::new(9, 9, 9, 9);
    draw(30, 3, |f| {
        content = kit::panel(
            f,
            Rect::new(0, 0, 30, 2),
            PanelSpec {
                footnote: Some(Line::from("note")),
                ..spec("Library", true)
            },
        )
    });
    assert_eq!(content.height, 0);
    assert_eq!(content.width, 28);
}

// ---------------------------------------------------------------------------------------------
// visible_window
// ---------------------------------------------------------------------------------------------

/// What a ratatui `List` of `len` one-row items does to `state` when drawn `height` rows tall.
fn list_state_after_render(len: usize, height: u16, state: &ListState) -> ListState {
    let mut state = state.clone();
    let items: Vec<&str> = vec!["x"; len];
    draw(10, height.max(1), |f| {
        f.render_stateful_widget(List::new(items), Rect::new(0, 0, 10, height), &mut state)
    });
    state
}

#[test]
fn visible_window_moves_the_offset_exactly_like_a_ratatui_list() {
    common::isolate();
    for len in [0usize, 1, 5, 12, 30] {
        for height in [0u16, 1, 4, 12] {
            for offset in [0usize, 3, 11, 40] {
                for selected in [None, Some(0), Some(4), Some(11), Some(29), Some(50)] {
                    let start = ListState::default().with_offset(offset).with_selected(selected);
                    let expected = list_state_after_render(len, height, &start);
                    let mut state = start.clone();
                    let window = kit::visible_window(&mut state, len, height as usize);
                    let case = format!("len {len} height {height} offset {offset} selected {selected:?}");
                    assert_eq!(state.offset(), expected.offset(), "offset, {case}");
                    assert_eq!(state.selected(), expected.selected(), "selected, {case}");
                    if height == 0 || len == 0 {
                        assert!(window.is_empty(), "{case}");
                    } else {
                        assert_eq!(window.start, state.offset(), "{case}");
                        assert_eq!(window.end, (window.start + height as usize).min(len), "{case}");
                        if let Some(sel) = state.selected() {
                            assert!(window.contains(&sel), "selected row visible, {case}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn visible_window_keeps_the_offset_while_the_selection_stays_on_screen() {
    common::isolate();
    let mut state = ListState::default().with_selected(Some(25));
    assert_eq!(kit::visible_window(&mut state, 40, 10), 16..26);
    state.select(Some(20));
    assert_eq!(kit::visible_window(&mut state, 40, 10), 16..26, "offset unchanged");
    state.select(Some(3));
    assert_eq!(kit::visible_window(&mut state, 40, 10), 3..13);
}

// ---------------------------------------------------------------------------------------------
// list_row
// ---------------------------------------------------------------------------------------------

fn file_row<'a>(index: &'a str, title: &'a str, artist: &'a str, time: &'a str) -> Vec<Col<'a>> {
    vec![
        Col::fixed(index, 4, Style::new().fg(P.dim)),
        Col::fixed(title, 12, Style::new()),
        Col::flex(artist, Style::new().fg(P.mid)),
        Col::auto(time, Style::new().fg(P.mid)),
    ]
}

#[test]
fn list_row_lays_out_fixed_flex_and_auto_columns() {
    common::isolate();
    let area = Rect::new(2, 0, 36, 1);
    let buf = draw(40, 1, |f| {
        kit::list_row(
            f,
            area,
            &file_row("01", "Tidewater", "Mira Lenz", "04:02"),
            RowMark::Plain,
        )
    });
    let row = tui::row(&buf, 0);
    assert_eq!(
        row,
        format!("  {:<4}{:<12}{:<15}04:02  ", "01", "Tidewater", "Mira Lenz")
    );
    assert_eq!(tui::cell_style(&buf, 2, 0).fg, Some(P.dim));
    assert_eq!(tui::cell_style(&buf, 18, 0).fg, Some(P.mid));
}

#[test]
fn list_row_cuts_long_text_with_an_ellipsis_and_keeps_a_gap_before_the_next_column() {
    common::isolate();
    let area = Rect::new(0, 0, 36, 1);
    let buf = draw(36, 1, |f| {
        kit::list_row(
            f,
            area,
            &file_row("01", "Northbound, slowly", "Ada Kowalczyk Ensemble", "06:02"),
            RowMark::Plain,
        )
    });
    let row = tui::row(&buf, 0);
    assert_eq!(row, "01  Northbound… Ada Kowalczyk… 06:02");
}

#[test]
fn list_row_columns_stay_aligned_with_wide_cjk_text() {
    common::isolate();
    let area = Rect::new(0, 0, 36, 3);
    let titles = ["Tidewater", "坂本龍一の夏の音楽", "海の歌"];
    let buf = draw(36, 3, |f| {
        for (i, title) in titles.iter().enumerate() {
            let row = Rect::new(area.x, area.y + i as u16, area.width, 1);
            kit::list_row(f, row, &file_row("01", title, "空", "04:02"), RowMark::Plain);
        }
    });
    for y in 0..3 {
        // The artist starts at column 16 and the time at 31 on every row.
        assert_eq!(buf[(16, y)].symbol(), "空", "row {y}: {:?}", tui::row(&buf, y));
        assert_eq!(buf[(30, y)].symbol(), " ", "row {y}");
        assert_eq!(buf[(31, y)].symbol(), "0", "row {y}");
    }
    // The 9-character (18-column) title is cut at a character boundary: 5 wide characters (10
    // columns), `…`, then the gap before the artist.
    assert_eq!(buf[(4, 1)].symbol(), "坂");
    assert_eq!(buf[(12, 1)].symbol(), "の");
    assert_eq!(buf[(14, 1)].symbol(), "…");
    assert_eq!(buf[(15, 1)].symbol(), " ");
}

#[test]
fn cursor_row_is_reverse_video_one_column_beyond_the_text_on_each_side() {
    common::isolate();
    let area = Rect::new(2, 0, 36, 1);
    let buf = draw(42, 1, |f| {
        kit::list_row(
            f,
            area,
            &file_row("04", "Afterimage", "Mira Lenz", "04:55"),
            RowMark::Cursor,
        )
    });
    let cursor = Style::new().fg(P.ground).bg(P.ink).add_modifier(Modifier::BOLD);
    for x in 1..=38 {
        let style = tui::cell_style(&buf, x, 0);
        assert_eq!(style.fg, cursor.fg, "column {x}");
        assert_eq!(style.bg, cursor.bg, "column {x}");
        assert!(style.add_modifier.contains(Modifier::BOLD), "column {x}");
    }
    assert_ne!(
        tui::cell_style(&buf, 0, 0).bg,
        Some(P.ink),
        "column 0 is outside the cursor"
    );
    assert_ne!(
        tui::cell_style(&buf, 39, 0).bg,
        Some(P.ink),
        "column 39 is outside the cursor"
    );
}

#[test]
fn cursor_row_at_the_screen_edge_is_clipped_to_the_buffer() {
    common::isolate();
    let buf = draw(20, 1, |f| {
        kit::list_row(
            f,
            Rect::new(0, 0, 20, 1),
            &file_row("1", "A", "B", "0:01"),
            RowMark::Cursor,
        )
    });
    assert_eq!(tui::cell_style(&buf, 0, 0).bg, Some(P.ink));
    assert_eq!(tui::cell_style(&buf, 19, 0).bg, Some(P.ink));
}

#[test]
fn marker_row_puts_a_triangle_in_the_first_column_without_reverse_video() {
    common::isolate();
    let cols = |marker: &'static str| {
        vec![
            Col::fixed(marker, 2, Style::new().fg(P.mid)),
            Col::fixed("06", 4, Style::new().fg(P.dim)),
            Col::flex("Northbound", Style::new()),
            Col::auto("06:02", Style::new().fg(P.mid)),
        ]
    };
    let buf = draw(30, 2, |f| {
        kit::list_row(f, Rect::new(1, 0, 28, 1), &cols(""), RowMark::Marker);
        kit::list_row(f, Rect::new(1, 1, 28, 1), &cols("▶"), RowMark::Plain);
    });
    assert_eq!(buf[(1, 0)].symbol(), "▸");
    assert_eq!(tui::cell_style(&buf, 1, 0).fg, Some(P.mid));
    assert!((0..30).all(|x| tui::cell_style(&buf, x, 0).bg != Some(P.ink)));
    assert_eq!(buf[(1, 1)].symbol(), "▶", "unmarked rows keep their first column");
    assert_eq!(tui::find_text(&buf, "06:02"), Some((24, 0)));
}
