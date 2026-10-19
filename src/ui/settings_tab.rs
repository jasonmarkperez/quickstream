//! Tab 4 Settings in the Telemetry style (mockup 2e at 120×40, width rules 3h): the SETTINGS
//! list, grouped PLAYBACK / LIBRARY / WEB in `SETTING_DISPLAY_ORDER`, and, from 110 columns, the
//! option panel for the selected setting beside it (its choices with ●/○, a description, the web
//! address and the keys that change it).
//!
//! Only what is in memory is drawn: the settings and their `AppUI::settings_context` (the Music
//! Homes the app holds, never bookmarks.json; the music folder once its startup worker has found
//! it; the home folder; the port the running web server bound) and the cached LAN address
//! (`SettingsManager::web_url`). The startup
//! home editor stays inline: while it is open its row shows the typed text with a block cursor.

use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

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

/// `lines` as ratatui wraps them in `width` columns (at spaces; a word wider than a line, such as
/// a URL, is broken at the edge), and how many rows that takes, so what follows can be placed
/// under it.
fn wrapped(lines: Vec<Line<'_>>, width: u16) -> (Paragraph<'_>, u16) {
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
    let rows = paragraph.line_count(width);
    (paragraph, u16::try_from(rows).unwrap_or(u16::MAX))
}

/// Rows the web address takes under a list `width` columns wide when there is room: one, or
/// two when it is wider than the list ([`web_address_rows`]).
fn address_rows_wanted(url: &str, width: u16) -> u16 {
    wrapped(vec![Line::raw(url)], width).1.clamp(1, 2)
}

/// The web address in DIM at the bottom of `list` (where neither the footnote nor the option
/// panel has room for it): on one row, or on two when it is wider than the list, leaving the list
/// at least one row (the cursor's); its start is cut with `…` only when the rows it gets are too
/// few. Returns the rows left for the list above it.
fn web_address_rows(f: &mut Frame, list: Rect, url: &str) -> Rect {
    if list.width == 0 {
        return list;
    }
    let max_rows = address_rows_wanted(url, list.width).min(list.height.saturating_sub(1));
    if max_rows == 0 {
        return list;
    }
    let shown = kit::visible_tail(url, usize::from(list.width) * usize::from(max_rows));
    let (paragraph, rows) = wrapped(vec![Line::styled(shown, theme::dim())], list.width);
    let rows = rows.min(max_rows);
    let area = Rect {
        y: list.bottom() - rows,
        height: rows,
        ..list
    };
    f.render_widget(paragraph, area.intersection(f.area()));
    Rect {
        height: list.height - rows,
        ..list
    }
}

impl AppUI {
    /// Tab 4: SETTINGS and, from 110 columns, the option panel (2e), which shows the web
    /// address on a web row. Under 110 columns the position (`05 / 12`) joins the footnote, and
    /// on a web row the web address takes the config file's place there (3h); at 80–89 columns,
    /// or under 24 rows, the footnote shrinks to the position on the label row and the web
    /// address goes under the list ([`web_address_rows`]), as it does from 110 columns when the
    /// option panel is too short to hold it whole, so it shows at every size.
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
        // The address is looked up only while a web row is selected (from the cache). With
        // network access on, a web row starts the LAN address lookup (on a worker) even while the
        // server is off, so the pairing link is ready once it is turned on.
        let web_row = setting::WEB.contains(&cursor);
        if web_row && self.settings_mgr.settings.web_server_lan {
            self.settings_mgr.prefetch_lan_address();
        }
        let web_url = if web_row {
            self.settings_mgr.web_url(self.web_port())
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
        // Under 24 rows the blank row under the rule goes when the list would not fit with it,
        // even without the blank rows between groups, together with the web address when it
        // goes under the list (3h); the option panel follows, to stay level.
        let header_rows = kit::panel_header_rows(compact, footnote.is_some(), false);
        let address_rows = web_url.as_deref().map_or(0, |url| {
            let under_list = if options_panel {
                // Under the list only when the option panel (with its blank row) can't hold it.
                let panel_rows = area.height.saturating_sub(kit::panel_header_rows(compact, true, true));
                wrapped(vec![Line::raw(url)], OPTION_COLS - kit::PANEL_RIGHT_PAD).1 > panel_rows
            } else {
                compact
            };
            if under_list {
                address_rows_wanted(url, kit::panel_text_width(list_area))
            } else {
                0
            }
        });
        let spacer = kit::keeps_spacer(
            screen.short,
            &[kit::PanelFit {
                area: Some(list_area),
                header_rows,
                rows: list_entries(false).len() + usize::from(address_rows),
            }],
        );
        let list = kit::panel_spaced(
            f,
            list_area,
            PanelSpec {
                label: "Settings",
                right: None,
                focused: true,
                footnote: footnote.map(Line::from),
                compact,
            },
            spacer,
        );
        let address_in_panel = options_panel && {
            let option_area = Rect::new(area.x + list_area.width + PANEL_GAP, area.y, OPTION_COLS, area.height);
            self.render_setting_options(f, option_area, &position, web_url.as_deref(), compact, spacer)
        };
        // Compact without the option panel the footnote is only the position, and the option
        // panel can be too short for the address: then the web address gets the list's last rows.
        let list = match &web_url {
            Some(url) if !address_in_panel && (options_panel || compact) => web_address_rows(f, list, url),
            _ => list,
        };
        self.render_settings_list(f, list);
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
                    // A path is cut from its start (`…/Karajan`), as in the editor and the
                    // footnote, so its last folder shows; other values (`LAUNCH DIRECTORY`
                    // too) from their end.
                    let path = index == setting::HOME_DIR && self.settings_mgr.settings.home_dir.is_some();
                    let fit = |value: &str, width: usize| {
                        if path {
                            kit::visible_tail(value, width)
                        } else {
                            kit::clip(value, width)
                        }
                    };
                    let value = match (&self.editing_home_dir, selected) {
                        (Some(input), true) if index == setting::HOME_DIR => {
                            format!("{}█", kit::visible_tail(input, value_cols.saturating_sub(1)))
                        }
                        (_, true) if value_cols >= 5 => {
                            let value = self.settings_mgr.short_value(index, &self.settings_context());
                            format!("◂ {} ▸", fit(&value, value_cols - 4))
                        }
                        _ => fit(
                            &self.settings_mgr.short_value(index, &self.settings_context()),
                            value_cols,
                        ),
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
    ///
    /// The address is shown whole or not at all: when the panel is short it comes before the
    /// description (which is cut at the panel's bottom) and the choices around the current one
    /// give it their rows. Returns whether the address is shown; if not, the caller draws it
    /// under the list.
    fn render_setting_options(
        &self,
        f: &mut Frame,
        area: Rect,
        position: &str,
        web_url: Option<&str>,
        compact: bool,
        spacer: bool,
    ) -> bool {
        let cursor = self.settings_cursor;
        let Some(def) = SETTING_DEFINITIONS.get(cursor) else {
            return false;
        };
        let list = kit::panel_spaced(
            f,
            area,
            PanelSpec {
                label: def.label,
                right: None,
                focused: true,
                footnote: Some(Line::from(position.to_string())),
                compact,
            },
            spacer,
        );
        let bold = theme::bold();

        // The rows the address takes, kept free of choices; none if it can't fit whole.
        let url_rows = web_url.map_or(0, |url| wrapped(vec![Line::raw(url)], list.width).1);
        let web_url = web_url.filter(|_| url_rows <= list.height && list.width > 0);
        let reserved = if web_url.is_some() { url_rows } else { 0 };
        let choice_room = match list.height - reserved {
            // A blank row between the choices and the address when there is room for both.
            room if reserved > 0 && room >= 2 => room - 1,
            room if reserved > 0 => room.min(1),
            room => room,
        };

        // The choices (each on one row), or while editing what the field takes.
        let editing = cursor == setting::HOME_DIR && self.editing_home_dir.is_some();
        let mut text = Vec::new();
        let mut choice_rows: u16 = 0;
        if editing {
            text.push(Line::styled(EDIT_HELP, theme::ink()));
        } else {
            let options = self.settings_mgr.options(cursor, &self.settings_context());
            // Too many choices for the panel: the ones around the current choice.
            let len = options.choices.len();
            let shown = len.min(usize::from(choice_room));
            let first = options.current.unwrap_or(0).saturating_sub(shown / 2).min(len - shown);
            for (i, choice) in options.choices.into_iter().enumerate().skip(first).take(shown) {
                let (mark, style) = if options.current == Some(i) {
                    (Col::fixed("●", MARK_COLS, bold), bold)
                } else {
                    (Col::fixed("○", MARK_COLS, theme::mid()), theme::mid())
                };
                let row = Rect::new(list.x, list.y + choice_rows, list.width, 1);
                kit::list_row(f, row, &[mark, Col::flex(choice.label, style)], RowMark::Plain);
                choice_rows += 1;
            }
        }
        // Then, after a blank row, the description and the web address on the web rows, wrapped.
        // When that is taller than the rows left, the address goes first, so only the
        // description is cut.
        let rows_left = list.height - choice_rows;
        let mut lines = text.clone();
        lines.push(Line::default());
        lines.push(Line::styled(def.description, theme::mid()));
        if let Some(url) = web_url {
            lines.push(Line::default());
            lines.push(Line::styled(url, theme::dim()));
        }
        let (mut paragraph, mut text_rows) = wrapped(lines, list.width);
        if let Some(url) = web_url.filter(|_| text_rows > rows_left) {
            if choice_rows > 0 && rows_left > url_rows {
                text.push(Line::default());
            }
            text.push(Line::styled(url, theme::dim()));
            text.push(Line::default());
            text.push(Line::styled(def.description, theme::mid()));
            (paragraph, text_rows) = wrapped(text, list.width);
        }
        let text_area = Rect {
            y: list.y + choice_rows,
            height: list.height - choice_rows,
            ..list
        };
        f.render_widget(paragraph, text_area.intersection(f.area()));

        // The keys that change this setting, as the key row shows them, after a blank row.
        let keys_dy = choice_rows.saturating_add(text_rows).saturating_add(1);
        if !editing && keys_dy < list.height {
            let hints: Vec<_> = self
                .tab_key_hints()
                .into_iter()
                .filter(|h| h.keys != "J K" && h.keys != "?")
                .collect();
            let row = Rect::new(list.x, list.y + keys_dy, list.width, 1);
            f.render_widget(kit::whole_key_row(&hints, list.width), row);
        }
        web_url.is_some()
    }
}
