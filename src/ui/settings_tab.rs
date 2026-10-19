//! Tab 4 Settings in the Telemetry style (mockup 2e at 120×40, width rules 3h): the SETTINGS
//! list, grouped PLAYBACK / LIBRARY / WEB in `SETTING_DISPLAY_ORDER`, and, from 110 columns, the
//! option panel for the selected setting beside it (its choices with ●/○, a description, the web
//! address and the keys that change it).
//!
//! Only what is in memory is drawn: the settings, the Music Homes the app holds
//! (`AppUI::bookmarks`, never bookmarks.json), the music folder looked up at startup, the port
//! the web server bound and the cached LAN address (`SettingsManager::web_url`). The startup
//! home editor stays inline: while it is open its row shows the typed text with a block cursor.

use ratatui::prelude::*;

use super::kit::{self, text_width, Col, PanelSpec, RowMark};
use super::queue_tab::{display_path, SEP};
use super::screen::{Screen, WidthTier};
use super::{theme, AppUI};
use crate::settings::{self, setting, SettingGroup, SETTING_DEFINITIONS, SETTING_DISPLAY_ORDER};

/// Width of the option panel (from 110 columns) and the gap before it (2e: 72 + 4 + 40 at 120).
const OPTION_COLS: u16 = 40;
const PANEL_GAP: u16 = 4;

/// The list keeps at least this many columns beside the option panel, or the panel is not drawn.
const MIN_LIST_COLS: u16 = 50;

/// The label column (2e); narrower lists split their width between label and value.
const LABEL_COLS: u16 = 32;
const MIN_VALUE_COLS: u16 = 16;

/// The option panel's ●/○ column.
const MARK_COLS: u16 = 4;

/// The option panel while the startup home editor is open (what `Enter` accepts: a path, `~`
/// for home, or nothing for the launch folder).
const EDIT_HELP: &str = "Type a folder (~ for home). Leave it empty for the launch folder.";

/// One row of the SETTINGS list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Heading(SettingGroup),
    Setting(usize),
    Blank,
}

/// The list's rows: each group's heading, then its settings, with a blank row between groups
/// when `spaced` (spacer rows go first on short terminals, 3h).
fn list_entries(spaced: bool) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut group = None;
    for &index in &SETTING_DISPLAY_ORDER {
        let this = SETTING_DEFINITIONS[index].group;
        if group != Some(this) {
            if spaced && group.is_some() {
                entries.push(Entry::Blank);
            }
            entries.push(Entry::Heading(this));
            group = Some(this);
        }
        entries.push(Entry::Setting(index));
    }
    entries
}

/// `05 / 12`: where setting `index` is listed, of how many.
fn position_note(index: usize) -> String {
    let n = SETTING_DISPLAY_ORDER.len();
    let pos = settings::display_position(index).map_or(0, |p| p + 1);
    format!("{pos:02} / {n:02}")
}

/// `text` broken into lines of at most `width` columns at spaces; a word wider than a line is
/// broken where it reaches the edge.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let sep = usize::from(!line.is_empty());
        if text_width(&line) + sep + text_width(word) <= width {
            if sep == 1 {
                line.push(' ');
            }
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        let mut pieces = wrap_chars(word, width);
        line = pieces.pop().unwrap_or_default();
        lines.extend(pieces);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `text` broken into lines of at most `width` columns anywhere (a URL has no spaces).
fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = kit::char_width(c);
        if used + w > width && used > 0 {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().expect("never empty").push(c);
        used += w;
    }
    lines
}

impl AppUI {
    /// Tab 4: SETTINGS and, from 110 columns, the option panel (2e). Under 110 columns the
    /// position (`05 / 12`) joins the footnote, and on a web row the web address takes the
    /// config file's place there (3h); at 80–89 columns, or under 24 rows, the footnote shrinks
    /// to the position on the label row.
    pub(super) fn render_settings_tab(&mut self, f: &mut Frame, area: Rect) {
        let screen = Screen::new(f.area());
        let compact = screen.tier < WidthTier::Medium || screen.short;
        let cursor = self.settings_cursor;
        let options_panel = screen.tier == WidthTier::Full && area.width >= OPTION_COLS + PANEL_GAP + MIN_LIST_COLS;
        let list_area = if options_panel {
            Rect {
                width: area.width - OPTION_COLS - PANEL_GAP,
                ..area
            }
        } else {
            area
        };
        // The address is looked up only while a web row is selected (from the cache).
        let web_url = if setting::WEB.contains(&cursor) {
            self.settings_mgr.web_url()
        } else {
            None
        };

        let position = position_note(cursor);
        let footnote = if compact {
            (!options_panel).then(|| position.clone())
        } else {
            // The text width of the list panel: 2 columns go to the ` ▪`.
            let room = usize::from(list_area.width.saturating_sub(2));
            let config = display_path(&self.settings_mgr.path, self.queue_tab.home.as_deref());
            let lead = match &web_url {
                Some(url) if !options_panel => url.clone(),
                _ => config,
            };
            Some(if options_panel {
                kit::visible_tail(&lead, room)
            } else {
                let tail = format!("{SEP}{position}");
                format!(
                    "{}{tail}",
                    kit::visible_tail(&lead, room.saturating_sub(text_width(&tail)))
                )
            })
        };
        let list = kit::panel(
            f,
            list_area,
            PanelSpec {
                label: "Settings",
                right: None,
                focused: true,
                footnote: footnote.map(Line::from),
                compact,
            },
        );
        self.render_settings_list(f, list);

        if options_panel {
            let option_area = Rect::new(area.x + list_area.width + PANEL_GAP, area.y, OPTION_COLS, area.height);
            self.render_setting_options(f, option_area, &position, web_url.as_deref(), compact);
        }
    }

    /// The grouped list: headings (DIM), each setting's label (MID) and value (INK); the
    /// selected one in the cursor bar as `◂ VALUE ▸`, or the startup home being typed. The blank
    /// rows between groups go when the list does not fit, then it scrolls to keep the cursor
    /// (and the heading of its group, for a group's first row) on screen.
    fn render_settings_list(&mut self, f: &mut Frame, list: Rect) {
        let height = usize::from(list.height);
        let mut entries = list_entries(true);
        if entries.len() > height {
            entries = list_entries(false);
        }
        let cursor = self.settings_cursor;
        let at = entries.iter().position(|e| *e == Entry::Setting(cursor)).unwrap_or(0);
        self.settings_state.select(Some(at));
        // Scrolled on a smaller terminal: no further than keeps the list's end at the bottom,
        // so a list that fits shows whole again.
        let max_offset = entries.len().saturating_sub(height);
        if self.settings_state.offset() > max_offset {
            *self.settings_state.offset_mut() = max_offset;
        }
        let mut window = kit::visible_window(&mut self.settings_state, entries.len(), height);
        if window.start == at && at > 0 && height >= 2 && matches!(entries[at - 1], Entry::Heading(_)) {
            window = at - 1..(at - 1 + height).min(entries.len());
            *self.settings_state.offset_mut() = window.start;
        }

        let label_cols = if list.width >= LABEL_COLS + MIN_VALUE_COLS {
            LABEL_COLS
        } else {
            (list.width / 2).max(1)
        };
        let value_cols = usize::from(list.width.saturating_sub(label_cols));
        for (dy, entry) in entries[window].iter().enumerate() {
            let row = Rect::new(list.x, list.y + dy as u16, list.width, 1);
            match *entry {
                Entry::Blank => {}
                Entry::Heading(group) => {
                    kit::list_row(f, row, &[Col::flex(group.label(), theme::dim())], RowMark::Plain);
                }
                Entry::Setting(index) => {
                    let selected = index == cursor;
                    let value = match (&self.editing_home_dir, selected) {
                        (Some(input), true) if index == setting::HOME_DIR => {
                            format!("{}█", kit::visible_tail(input, value_cols.saturating_sub(1)))
                        }
                        (_, true) if value_cols >= 5 => {
                            let value = self.settings_mgr.short_value(index);
                            format!("◂ {} ▸", kit::clip(&value, value_cols - 4))
                        }
                        _ => self.settings_mgr.short_value(index),
                    };
                    let cols = [
                        Col::fixed(kit::upper(SETTING_DEFINITIONS[index].label), label_cols, theme::mid()),
                        Col::flex(value, theme::ink()),
                    ];
                    let mark = if selected { RowMark::Cursor } else { RowMark::Plain };
                    kit::list_row(f, row, &cols, mark);
                }
            }
        }
    }

    /// The option panel (from 110 columns): the selected setting's name, its position in the
    /// footnote, every choice it cycles through (● the one it holds), its description, the web
    /// address on the web rows, and the keys that change it. While the startup home editor is
    /// open: what the field takes, instead of the choices.
    fn render_setting_options(&self, f: &mut Frame, area: Rect, position: &str, web_url: Option<&str>, compact: bool) {
        let cursor = self.settings_cursor;
        let Some(def) = SETTING_DEFINITIONS.get(cursor) else {
            return;
        };
        let list = kit::panel(
            f,
            area,
            PanelSpec {
                label: def.label,
                right: None,
                focused: true,
                footnote: Some(Line::from(position.to_string())),
                compact,
            },
        );
        let width = usize::from(list.width);
        let height = usize::from(list.height);
        let bold = theme::ink().add_modifier(Modifier::BOLD);
        let mut rows: Vec<Vec<Col>> = Vec::new();

        let editing = cursor == setting::HOME_DIR && self.editing_home_dir.is_some();
        if editing {
            rows.extend(
                wrap_words(EDIT_HELP, width)
                    .into_iter()
                    .map(|line| vec![Col::flex(line, theme::ink())]),
            );
        } else {
            let options = self
                .settings_mgr
                .options(cursor, &self.bookmarks, settings::music_dir());
            // Too many choices for the panel: the ones around the current choice.
            let len = options.choices.len();
            let shown = len.min(height);
            let first = options.current.unwrap_or(0).saturating_sub(shown / 2).min(len - shown);
            for (i, choice) in options.choices.into_iter().enumerate().skip(first).take(shown) {
                let (mark, style) = if options.current == Some(i) {
                    (Col::fixed("●", MARK_COLS, bold), bold)
                } else {
                    (Col::fixed("○", MARK_COLS, theme::mid()), theme::mid())
                };
                rows.push(vec![mark, Col::flex(choice.label, style)]);
            }
        }
        rows.push(Vec::new());
        rows.extend(
            wrap_words(def.description, width)
                .into_iter()
                .map(|line| vec![Col::flex(line, theme::mid())]),
        );
        if let Some(url) = web_url {
            rows.push(Vec::new());
            rows.extend(
                wrap_chars(url, width)
                    .into_iter()
                    .map(|line| vec![Col::flex(line, theme::dim())]),
            );
        }

        for (dy, cols) in rows.iter().take(height).enumerate() {
            let row = Rect::new(list.x, list.y + dy as u16, list.width, 1);
            kit::list_row(f, row, cols, RowMark::Plain);
        }
        // The keys that change this setting, as the key row shows them, after a blank row.
        let keys_dy = rows.len() + 1;
        if !editing && keys_dy < height {
            let hints: Vec<_> = self
                .tab_key_hints()
                .into_iter()
                .filter(|h| h.keys != "J K" && h.keys != "?")
                .collect();
            let row = Rect::new(list.x, list.y + keys_dy as u16, list.width, 1);
            f.render_widget(kit::key_row(&hints, list.width, None), row);
        }
    }
}
