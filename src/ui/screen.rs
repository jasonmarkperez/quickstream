//! Terminal size classes the Telemetry layout adapts to (width rules, mockup 3h).

use ratatui::layout::Rect;

/// Width class of the terminal, narrowest first, so `tier >= WidthTier::Medium` reads naturally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WidthTier {
    /// Under 80 columns: tabs 2 and 5 show only the focused pane.
    Narrow,
    /// 80–89: the artist column drops, the strip shows the title only, keys are trimmed to 5.
    Compact,
    /// 90–109: side panels hide, `QUEUE & LIBRARY` becomes `QUEUE`.
    Medium,
    /// 110 and wider: full layout with side panels.
    Full,
}

impl WidthTier {
    pub fn from_width(width: u16) -> Self {
        match width {
            0..=79 => Self::Narrow,
            80..=89 => Self::Compact,
            90..=109 => Self::Medium,
            _ => Self::Full,
        }
    }
}

/// Size facts for one frame, computed once from the frame area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub tier: WidthTier,
    /// Fewer than 24 rows: Up Next and spacer rows go first, the header folds into the tab row.
    pub short: bool,
    pub area: Rect,
}

impl Screen {
    pub fn new(area: Rect) -> Self {
        Self {
            tier: WidthTier::from_width(area.width),
            short: area.height < 24,
            area,
        }
    }
}

impl From<Rect> for Screen {
    fn from(area: Rect) -> Self {
        Self::new(area)
    }
}
