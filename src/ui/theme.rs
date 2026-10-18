//! Telemetry colour tokens and the styles built from them.
//!
//! Six tokens (style sheet 2a): GROUND is the background of every cell, INK primary text and
//! focus, MID secondary text, DIM labels and indices, RULE rules and keycaps, ERROR errors and the
//! delete confirmation only. Terminals that announce 24-bit colour (`COLORTERM=truecolor` or
//! `24bit`) get the RGB values; every other terminal gets the nearest 256-colour greys. The choice
//! is made once at startup with [`init`]; until then (and in tests) the RGB palette is used.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

/// How many colours the terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit RGB.
    TrueColor,
    /// The 256-colour palette.
    Indexed,
}

impl ColorDepth {
    /// Depth for a `COLORTERM` value: `truecolor` or `24bit` (any case) mean RGB, anything else
    /// (or no value) means 256 colours.
    pub fn from_colorterm(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit") => Self::TrueColor,
            _ => Self::Indexed,
        }
    }

    /// Depth of the terminal this process runs in, from the `COLORTERM` environment variable.
    pub fn detect() -> Self {
        Self::from_colorterm(std::env::var("COLORTERM").ok().as_deref())
    }
}

/// The six colour tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub ground: Color,
    pub ink: Color,
    pub mid: Color,
    pub dim: Color,
    pub rule: Color,
    pub error: Color,
}

impl Palette {
    /// The style sheet's RGB values.
    pub const TRUECOLOR: Palette = Palette {
        ground: Color::Rgb(0x14, 0x14, 0x13),
        ink: Color::Rgb(0xE8, 0xE8, 0xE4),
        mid: Color::Rgb(0x9A, 0x9A, 0x95),
        dim: Color::Rgb(0x5F, 0x5F, 0x5B),
        rule: Color::Rgb(0x2E, 0x2E, 0x2C),
        error: Color::Rgb(0xED, 0x74, 0x73),
    };

    /// Nearest entries of the 256-colour palette (greys from the 232–255 ramp).
    pub const INDEXED: Palette = Palette {
        ground: Color::Indexed(234),
        ink: Color::Indexed(254),
        mid: Color::Indexed(247),
        dim: Color::Indexed(240),
        rule: Color::Indexed(236),
        error: Color::Indexed(210),
    };

    pub fn for_depth(depth: ColorDepth) -> &'static Palette {
        match depth {
            ColorDepth::TrueColor => &Self::TRUECOLOR,
            ColorDepth::Indexed => &Self::INDEXED,
        }
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Choose the palette for `depth`. Only the first call has an effect; returns whether this call
/// set it.
pub fn init(depth: ColorDepth) -> bool {
    PALETTE.set(*Palette::for_depth(depth)).is_ok()
}

/// The palette chosen by [`init`], or [`Palette::TRUECOLOR`] if it was never called.
pub fn palette() -> &'static Palette {
    PALETTE.get().unwrap_or(&Palette::TRUECOLOR)
}

/// Every cell: INK on GROUND.
pub fn base() -> Style {
    let p = palette();
    Style::new().fg(p.ink).bg(p.ground)
}

pub fn ink() -> Style {
    Style::new().fg(palette().ink)
}

pub fn mid() -> Style {
    Style::new().fg(palette().mid)
}

pub fn dim() -> Style {
    Style::new().fg(palette().dim)
}

pub fn rule() -> Style {
    Style::new().fg(palette().rule)
}

pub fn error() -> Style {
    Style::new().fg(palette().error)
}

/// Panel label: INK bold when its panel has focus, MID otherwise.
pub fn label(focused: bool) -> Style {
    if focused {
        ink().add_modifier(Modifier::BOLD)
    } else {
        mid()
    }
}

/// Cursor row in the focused pane and the active tab chip. The colours are explicit rather than
/// `REVERSED`, so the result does not depend on the style the cell had before.
pub fn cursor() -> Style {
    let p = palette();
    Style::new().fg(p.ground).bg(p.ink).add_modifier(Modifier::BOLD)
}

/// A key name in a key hint (` SPC `).
pub fn keycap() -> Style {
    let p = palette();
    Style::new().fg(p.ink).bg(p.rule)
}
