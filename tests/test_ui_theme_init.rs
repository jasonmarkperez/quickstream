//! `theme::init` picks the palette once per process. It lives in its own test binary so the
//! other UI tests keep the deterministic TRUECOLOR default.

mod common;

use ratatui::style::Style;

use quickstream::ui::theme::{self, ColorDepth, Palette};

#[test]
fn init_selects_the_palette_once() {
    common::isolate();
    assert_eq!(theme::palette(), &Palette::TRUECOLOR);

    assert!(theme::init(ColorDepth::Indexed));
    assert_eq!(theme::palette(), &Palette::INDEXED);
    let p = Palette::INDEXED;
    assert_eq!(theme::base(), Style::new().fg(p.ink).bg(p.ground));
    assert_eq!(theme::keycap(), Style::new().fg(p.ink).bg(p.rule));

    // Decided once at startup: a later call changes nothing.
    assert!(!theme::init(ColorDepth::TrueColor));
    assert_eq!(theme::palette(), &Palette::INDEXED);
}
