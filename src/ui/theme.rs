//! Telemetry colour tokens and the styles built from them.
//!
//! Six tokens (style sheet 2a): GROUND is the background of every cell, INK primary text and
//! focus, MID secondary text, DIM labels and indices, RULE rules and keycaps, ERROR errors and the
//! delete confirmation only. Terminals that announce 24-bit colour (`COLORTERM=truecolor` or
//! `24bit`) get the RGB values; 16-colour terminals (`TERM=linux`, `*-16color`) the named ANSI
//! colours; with `NO_COLOR` set, or on an 8-colour or monochrome terminal (`*-8color`, `vt100`,
//! `dumb` …), no colour at all, and reverse video, bold and faint carry what colour would; every
//! other terminal gets the nearest 256-colour greys. The choice is made once at startup with
//! [`init`]; until then (and in tests) the RGB palette is used.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

/// How many colours the terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit RGB.
    TrueColor,
    /// The 256-colour palette.
    Indexed,
    /// The 16 named ANSI colours (the Linux console, `*-16color`).
    Basic,
    /// No colour at all (`NO_COLOR`; 8-colour and monochrome terminals: `*-8color`, `vt100`,
    /// `dumb` …).
    Mono,
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

    /// Depth for the `COLORTERM` and `TERM` values and whether `NO_COLOR` is set (see
    /// [`ColorDepth::no_color`]). `NO_COLOR` wins (crossterm then writes no colours anyway), then a
    /// `COLORTERM` announcing 24-bit colour; a `TERM` that shows 16 colours means
    /// [`ColorDepth::Basic`], one that shows 8 or none [`ColorDepth::Mono`]; anything else 256
    /// colours, as plain `xterm`, `screen` and `tmux` usually show them.
    pub fn from_env(colorterm: Option<&str>, term: Option<&str>, no_color: bool) -> Self {
        if no_color {
            return Self::Mono;
        }
        match Self::from_colorterm(colorterm) {
            Self::TrueColor => Self::TrueColor,
            _ => match term.map(TermColours::of) {
                Some(TermColours::Sixteen) => Self::Basic,
                Some(TermColours::Few) => Self::Mono,
                _ => Self::Indexed,
            },
        }
    }

    /// Whether a `NO_COLOR` value asks for no colour: set and not empty (no-color.org), as
    /// crossterm reads it.
    pub fn no_color(value: Option<&str>) -> bool {
        value.is_some_and(|v| !v.is_empty())
    }

    /// Depth of the terminal this process runs in, from the `COLORTERM`, `TERM` and `NO_COLOR`
    /// environment variables.
    pub fn detect() -> Self {
        let var = |name| std::env::var(name).ok();
        Self::from_env(
            var("COLORTERM").as_deref(),
            var("TERM").as_deref(),
            Self::no_color(var("NO_COLOR").as_deref()),
        )
    }
}

/// What a `TERM` value says about the colours its terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TermColours {
    /// The 16 named colours, read from the 256-colour indexes crossterm writes for them
    /// (`ESC[38;5;Nm`, even for the named ones): the Linux console (and `screen.linux`, screen
    /// running on it) and any `*-16color` entry.
    Sixteen,
    /// 8 colours or none: `*-8color`, `xterm-color`, the `ansi`, `vt52` … `vt320`, `cons25` and
    /// `dumb` families and the monochrome Linux console (`linux-m`). They cannot show the
    /// 256-colour indexes (and with 8 colours DIM, index 8, can fall back to black on the black
    /// ground), so they get no colour at all.
    Few,
    /// Probably 256 (plain `xterm`, `screen`, `tmux`, any `*256color` entry).
    Many,
}

impl TermColours {
    fn of(term: &str) -> Self {
        let term = term.trim().to_ascii_lowercase();
        let family = term.split('-').next().unwrap_or_default();
        if term.contains("256color") {
            Self::Many
        } else if term.ends_with("-8color")
            || term == "xterm-color"
            || term.starts_with("linux-m")
            || matches!(
                family,
                "ansi" | "cons25" | "dumb" | "vt52" | "vt100" | "vt102" | "vt220" | "vt320"
            )
        {
            Self::Few
        } else if term.ends_with("-16color") || term.ends_with(".linux") || family == "linux" {
            Self::Sixteen
        } else {
            Self::Many
        }
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
    /// Text of the screen under a modal ([`Palette::dimmed`]): RULE, or DIM where RULE is a
    /// colour that would stand out (Basic's blue).
    pub under_modal: Color,
    /// No colour at all ([`ColorDepth::Mono`]): every token is the terminal's own colour, and the
    /// styles that need colour to be seen use reverse video, bold and faint instead.
    pub mono: bool,
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
        under_modal: Color::Rgb(0x2E, 0x2E, 0x2C),
        mono: false,
    };

    /// Nearest entries of the 256-colour palette (greys from the 232–255 ramp).
    pub const INDEXED: Palette = Palette {
        ground: Color::Indexed(234),
        ink: Color::Indexed(254),
        mid: Color::Indexed(247),
        dim: Color::Indexed(240),
        rule: Color::Indexed(236),
        error: Color::Indexed(210),
        under_modal: Color::Indexed(236),
        mono: false,
    };

    /// The named ANSI colours. There are only four greys, so INK, MID and DIM take bright white,
    /// white and bright black, and RULE is blue. The tokens drawn as backgrounds (GROUND; RULE
    /// under keycaps; INK under the cursor bar and chips; ERROR under the delete chip) are kept
    /// apart even where a bright background turns into its normal colour (the Linux console):
    /// a bright-black RULE would vanish into the black ground there. The screen under a modal is
    /// DIM, not the blue RULE, so it stays behind the modal's own DIM text.
    pub const BASIC: Palette = Palette {
        ground: Color::Black,
        ink: Color::White,
        mid: Color::Gray,
        dim: Color::DarkGray,
        rule: Color::Blue,
        error: Color::Red,
        under_modal: Color::DarkGray,
        mono: false,
    };

    /// No colour at all: every token is the terminal's own colour.
    pub const MONO: Palette = Palette {
        ground: Color::Reset,
        ink: Color::Reset,
        mid: Color::Reset,
        dim: Color::Reset,
        rule: Color::Reset,
        error: Color::Reset,
        under_modal: Color::Reset,
        mono: true,
    };

    pub fn for_depth(depth: ColorDepth) -> &'static Palette {
        match depth {
            ColorDepth::TrueColor => &Self::TRUECOLOR,
            ColorDepth::Indexed => &Self::INDEXED,
            ColorDepth::Basic => &Self::BASIC,
            ColorDepth::Mono => &Self::MONO,
        }
    }

    /// Every cell: INK on GROUND.
    pub fn base(&self) -> Style {
        Style::new().fg(self.ink).bg(self.ground)
    }

    pub fn ink(&self) -> Style {
        self.not_faint(Style::new().fg(self.ink))
    }

    pub fn mid(&self) -> Style {
        self.not_faint(Style::new().fg(self.mid))
    }

    /// DIM; faint in Mono, so it stays apart from MID.
    pub fn dim(&self) -> Style {
        self.faint(Style::new().fg(self.dim))
    }

    /// RULE; faint in Mono.
    pub fn rule(&self) -> Style {
        self.faint(Style::new().fg(self.rule))
    }

    /// ERROR; bold in Mono.
    pub fn error(&self) -> Style {
        let style = self.not_faint(Style::new().fg(self.error));
        if self.mono {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    /// Bold INK: titles, names and numbers that stand out (a modal's title row, the selected track).
    pub fn bold(&self) -> Style {
        self.ink().add_modifier(Modifier::BOLD)
    }

    /// Panel label: INK bold when its panel has focus, MID otherwise.
    pub fn label(&self, focused: bool) -> Style {
        if focused {
            self.bold()
        } else {
            self.mid()
        }
    }

    /// Cursor row in the focused pane and the active tab chip. The colours are explicit rather than
    /// `REVERSED`, so the result does not depend on the style the cell had before; in Mono, with no
    /// colours to name, it is reverse video and never faint.
    pub fn cursor(&self) -> Style {
        self.reversed(Style::new().fg(self.ground).bg(self.ink).add_modifier(Modifier::BOLD))
    }

    /// A modal's title chip (` KEYS `): reverse video like [`Palette::cursor`], or GROUND on ERROR
    /// for an error or the delete confirmation (in Mono bold reverse video and underlined, so it
    /// reads apart from the other chips without colour, with the error border in bold).
    pub fn chip(&self, error: bool) -> Style {
        if !error {
            return self.cursor();
        }
        let style = self.reversed(Style::new().fg(self.ground).bg(self.error).add_modifier(Modifier::BOLD));
        if self.mono {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }

    /// A key name in a key hint (` SPC `): INK on RULE, reverse video in Mono.
    pub fn keycap(&self) -> Style {
        self.reversed(Style::new().fg(self.ink).bg(self.rule))
    }

    /// The screen under a modal (`kit::dim_buffer`): RULE on GROUND (DIM in Basic, where RULE is
    /// blue), faint in Mono.
    pub fn dimmed(&self) -> Style {
        self.faint(Style::new().fg(self.under_modal).bg(self.ground))
    }

    /// `style` made faint in Mono, where no colour sets it apart. Like a new colour replacing the
    /// old one, it also clears the reverse video and bold that stand in for a background and for
    /// INK, so it reads faint whatever the cell held before.
    fn faint(&self, style: Style) -> Style {
        if self.mono {
            style
                .add_modifier(Modifier::DIM)
                .remove_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            style
        }
    }

    /// `style` cleared of faint in Mono, where faint stands in for a colour: a style drawn over a
    /// faint cell (the key row over the screen under a modal, a footnote's spans over DIM) is not
    /// faint, as a new colour would replace the old one.
    fn not_faint(&self, style: Style) -> Style {
        if self.mono {
            style.remove_modifier(Modifier::DIM)
        } else {
            style
        }
    }

    /// `style` in reverse video (and never faint) in Mono, where its background has no colour to
    /// show. The one use of `REVERSED`: the coloured palettes name their colours.
    fn reversed(&self, style: Style) -> Style {
        if self.mono {
            self.not_faint(style.add_modifier(Modifier::REVERSED))
        } else {
            style
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

/// Every cell: INK on GROUND ([`Palette::base`]).
pub fn base() -> Style {
    palette().base()
}

pub fn ink() -> Style {
    palette().ink()
}

pub fn mid() -> Style {
    palette().mid()
}

/// DIM ([`Palette::dim`]; faint in Mono).
pub fn dim() -> Style {
    palette().dim()
}

/// RULE ([`Palette::rule`]; faint in Mono).
pub fn rule() -> Style {
    palette().rule()
}

/// ERROR ([`Palette::error`]; bold in Mono).
pub fn error() -> Style {
    palette().error()
}

/// Bold INK: titles, names and numbers that stand out (a modal's title row, the selected track).
pub fn bold() -> Style {
    palette().bold()
}

/// Panel label: INK bold when its panel has focus, MID otherwise.
pub fn label(focused: bool) -> Style {
    palette().label(focused)
}

/// Cursor row in the focused pane and the active tab chip ([`Palette::cursor`]).
pub fn cursor() -> Style {
    palette().cursor()
}

/// A modal's title chip ([`Palette::chip`]).
pub fn chip(error: bool) -> Style {
    palette().chip(error)
}

/// A key name in a key hint (` SPC `, [`Palette::keycap`]).
pub fn keycap() -> Style {
    palette().keycap()
}

/// The screen under a modal ([`Palette::dimmed`]).
pub fn dimmed() -> Style {
    palette().dimmed()
}
