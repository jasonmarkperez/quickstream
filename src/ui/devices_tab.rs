//! Tab 3 Devices in the Telemetry style (mockup 2d at 120×40, width rules 3h): the OUTPUT
//! DEVICES table (Local Audio, then each discovered renderer) and, from 110 columns, the DEVICE
//! panel for the selected row beside it.
//!
//! Only what discovery already holds in memory is drawn (the device list, the scanning flag and
//! the active output from [`DeviceManager`]), plus the volume of the active output, which the
//! player already reports for the strip. Nothing is fetched for the render, and an inactive
//! renderer's volume is never shown: the app doesn't know it. A field without data is left out.
//!
//! [`DeviceManager`]: crate::discovery::DeviceManager

use ratatui::prelude::*;

use super::kit::{self, text_width, Col, PanelSpec, RowMark};
use super::queue_tab::SEP;
use super::screen::{Screen, WidthTier};
use super::{theme, AppUI};
use crate::discovery::{DLNADevice, SSDP_MULTICAST_ADDR};

/// Width of the DEVICE panel (from 110 columns) and the gap before it (2d: 72 + 4 + 40 at 120).
const DETAIL_COLS: u16 = 40;
const PANEL_GAP: u16 = 4;

/// The table keeps at least this many columns beside the DEVICE panel, or the panel is not drawn.
const MIN_TABLE_COLS: u16 = 40;

/// Table columns (2d): the `●` / `○` marker, then type, address and `ACTIVE`; the name takes the
/// rest (30 columns at 120).
const MARK_COLS: u16 = 4;
const TYPE_COLS: u16 = 10;
const ADDRESS_COLS: u16 = 20;
const ACTIVE_COLS: u16 = 6;

/// The name column keeps at least this many columns: narrower, the `ACTIVE`, type and address
/// columns drop, in that order (the `●` already marks the active output).
const MIN_NAME_COLS: u16 = 12;

/// Width of a DEVICE row's key column (`NAME`, `HOST`, …).
const KEY_COLS: u16 = 12;

/// Under the table while discovery runs, after the spinner.
const SEARCHING: &str = "SEARCHING FOR DLNA / UPNP MEDIARENDERERS";

/// Under the table once discovery has found nothing.
const NONE_FOUND: &str = "NO RENDERERS FOUND";

/// One row of the table.
struct OutputRow {
    name: String,
    kind: String,
    address: String,
    active: bool,
}

/// Which optional table columns fit beside a name of at least [`MIN_NAME_COLS`].
#[derive(Clone, Copy)]
struct TableColumns {
    kind: bool,
    address: bool,
    active: bool,
}

impl TableColumns {
    fn for_width(width: u16) -> Self {
        let mut cols = Self {
            kind: true,
            address: true,
            active: true,
        };
        let fixed = |c: &Self| {
            MARK_COLS
                + if c.kind { TYPE_COLS } else { 0 }
                + if c.address { ADDRESS_COLS } else { 0 }
                + if c.active { ACTIVE_COLS } else { 0 }
        };
        let drops: [fn(&mut Self); 3] = [|c| c.active = false, |c| c.kind = false, |c| c.address = false];
        for drop in drops {
            if width >= fixed(&cols) + MIN_NAME_COLS {
                break;
            }
            drop(&mut cols);
        }
        cols
    }

    /// The row's columns: marker, name, and the optional ones that fit.
    fn cols<'a>(self, mark: (&'a str, Style), name: (&'a str, Style), rest: [(&'a str, Style); 3]) -> Vec<Col<'a>> {
        let [kind, address, active] = rest;
        let mut cols = vec![Col::fixed(mark.0, MARK_COLS, mark.1), Col::flex(name.0, name.1)];
        if self.kind {
            cols.push(Col::fixed(kind.0, TYPE_COLS, kind.1));
        }
        if self.address {
            cols.push(Col::fixed(address.0, ADDRESS_COLS, address.1));
        }
        if self.active {
            cols.push(Col::fixed(active.0, ACTIVE_COLS, active.1));
        }
        cols
    }
}

/// `MAKER MODEL` of a renderer, or `None` when discovery found neither; a model that only
/// repeats the name is left out, as [`DLNADevice::display_label`] does.
fn maker_model(device: &DLNADevice) -> Option<String> {
    let mut parts = Vec::new();
    if !device.manufacturer.is_empty() {
        parts.push(device.manufacturer.as_str());
    }
    if !device.model_name.is_empty() && device.model_name != device.friendly_name {
        parts.push(device.model_name.as_str());
    }
    (!parts.is_empty()).then(|| kit::upper(&parts.join(" ")))
}

/// `text` in at most `width` columns, cut in the middle with `…` so both ends stay (a UDN's
/// start and its last digits are what tell two apart).
fn middle_cut(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    if width < 3 {
        return kit::clip(text, width);
    }
    let keep = width - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = text.chars().collect();
    let start: String = chars[..head.min(chars.len())].iter().collect();
    let end: String = chars[chars.len().saturating_sub(tail)..].iter().collect();
    kit::clip(&format!("{start}…{end}"), width)
}

/// The path of a control URL (`/AVTransport/control`), or the URL itself if it doesn't parse.
fn url_path(url: &str) -> String {
    crate::discovery::parse_http_url(url)
        .map(|u| u.path)
        .unwrap_or_else(|| url.to_string())
}

impl AppUI {
    /// Tab 3: OUTPUT DEVICES and, from 110 columns, the DEVICE panel (2d). Under 110 columns the
    /// selected renderer's maker and model join the footnote (3h); at 80–89 columns, or under 24
    /// rows, the footnote shrinks to the count (and the scanning note) on the label row.
    pub(super) fn render_devices_tab(&mut self, f: &mut Frame, area: Rect) {
        let screen = Screen::new(f.area());
        let compact = screen.tier < WidthTier::Medium || screen.short;
        let scanning = self.device_manager.is_scanning();
        let devices = self.device_manager.get_devices();
        let active = self.device_manager.get_active_device();
        let selected = self.devices_cursor.min(devices.len());
        let selected_device = selected.checked_sub(1).and_then(|i| devices.get(i));

        let detail = screen.tier == WidthTier::Full && area.width >= DETAIL_COLS + PANEL_GAP + MIN_TABLE_COLS;
        let table_area = if detail {
            Rect {
                width: area.width - DETAIL_COLS - PANEL_GAP,
                ..area
            }
        } else {
            area
        };

        let scanning_note = format!("SCANNING {}", kit::spinner(self.ui_epoch.elapsed()));
        let found = format!("{} FOUND", devices.len());
        let footnote = if compact {
            if scanning {
                format!("{found}{SEP}{scanning_note}")
            } else {
                found
            }
        } else {
            let mut parts = vec![found, format!("SSDP {SSDP_MULTICAST_ADDR}")];
            if !detail {
                parts.extend(selected_device.and_then(maker_model));
            }
            parts.join(SEP)
        };
        let list = kit::panel(
            f,
            table_area,
            PanelSpec {
                label: "Output devices",
                right: scanning.then_some(scanning_note.as_str()),
                focused: true,
                footnote: Some(Line::from(footnote)),
                compact,
            },
        );
        self.render_device_table(f, list, &devices, active.as_ref(), scanning);

        if detail {
            let detail_area = Rect::new(area.x + table_area.width + PANEL_GAP, area.y, DETAIL_COLS, area.height);
            let is_active = match (selected_device, &active) {
                (None, None) => true,
                (Some(dev), Some(act)) => dev.udn == act.udn,
                _ => false,
            };
            self.render_device_detail(f, detail_area, selected_device, is_active, compact);
        }
    }

    /// The table: column names, a row per output (double-spaced while every row fits that way,
    /// 2d), then the scanning or nothing-found line when there is room for it. Only the rows on
    /// screen are drawn.
    fn render_device_table(
        &mut self,
        f: &mut Frame,
        list: Rect,
        devices: &[DLNADevice],
        active: Option<&DLNADevice>,
        scanning: bool,
    ) {
        let len = devices.len() + 1;
        // Column names and the blank under them, when there is a row left for the outputs.
        let header_rows: u16 = if list.height >= 3 { 2 } else { 0 };
        let avail = list.height - header_rows;
        let message = if scanning {
            Some(SEARCHING)
        } else if devices.is_empty() {
            Some(NONE_FOUND)
        } else {
            None
        };
        // The message (after a blank row) only when every output still gets a row.
        let message_rows: u16 = if message.is_some() && usize::from(avail) >= len + 2 {
            2
        } else {
            0
        };
        let rows_height = usize::from(avail - message_rows);
        let pitch: u16 = if rows_height >= 2 * len - 1 { 2 } else { 1 };
        let shown = if pitch == 2 {
            rows_height.div_ceil(2)
        } else {
            rows_height
        };

        self.devices_state.select(Some(self.devices_cursor));
        let window = kit::visible_window(&mut self.devices_state, len, shown);
        let columns = TableColumns::for_width(list.width);
        let row_area = |dy: u16| Rect::new(list.x, list.y + dy, list.width, 1);

        if header_rows > 0 {
            let dim = theme::dim();
            let cols = columns.cols(("", dim), ("NAME", dim), [("TYPE", dim), ("ADDRESS", dim), ("", dim)]);
            kit::list_row(f, row_area(0), &cols, RowMark::Plain);
        }

        let local_kind = kit::upper(&self.local_backend());
        let mut last_dy = header_rows;
        for (n, idx) in window.clone().enumerate() {
            let row = match idx.checked_sub(1).and_then(|i| devices.get(i)) {
                None => OutputRow {
                    name: "LOCAL AUDIO".to_string(),
                    kind: local_kind.clone(),
                    address: "THIS MACHINE".to_string(),
                    active: active.is_none(),
                },
                Some(dev) => OutputRow {
                    name: kit::upper(&dev.friendly_name),
                    kind: "DLNA".to_string(),
                    address: format!("{}:{}", dev.host, dev.port),
                    active: active.is_some_and(|a| a.udn == dev.udn),
                },
            };
            let bold = theme::ink().add_modifier(Modifier::BOLD);
            let (mark, name_style) = if row.active {
                (("●", bold), bold)
            } else {
                (("○", theme::mid()), theme::ink())
            };
            let cols = columns.cols(
                mark,
                (&row.name, name_style),
                [
                    (&row.kind, theme::mid()),
                    (&row.address, theme::mid()),
                    (if row.active { "ACTIVE" } else { "" }, bold),
                ],
            );
            let mark = if idx == self.devices_cursor {
                RowMark::Cursor
            } else {
                RowMark::Plain
            };
            let dy = header_rows + n as u16 * pitch;
            kit::list_row(f, row_area(dy), &cols, mark);
            last_dy = dy;
        }

        if let (Some(message), 2) = (message, message_rows) {
            let glyph = if scanning {
                kit::spinner(self.ui_epoch.elapsed()).to_string()
            } else {
                String::new()
            };
            let cols = [
                Col::fixed(glyph, MARK_COLS, theme::dim()),
                Col::flex(message, theme::dim()),
            ];
            kit::list_row(f, row_area(last_dy + 2), &cols, RowMark::Plain);
        }
    }

    /// The DEVICE panel (from 110 columns): the selected output's type in the footnote, then a
    /// row per field discovery found, its status, its volume when it is the active output (the
    /// player's, as on the strip), and for an output not in use the key that switches to it.
    fn render_device_detail(
        &self,
        f: &mut Frame,
        area: Rect,
        device: Option<&DLNADevice>,
        is_active: bool,
        compact: bool,
    ) {
        let footnote = if device.is_some() {
            "DLNA MEDIARENDERER"
        } else {
            "LOCAL PLAYBACK"
        };
        let list = kit::panel(
            f,
            area,
            PanelSpec {
                label: "Device",
                right: None,
                focused: true,
                footnote: Some(Line::from(footnote)),
                compact,
            },
        );

        let value_cols = usize::from(list.width.saturating_sub(KEY_COLS));
        let mut fields: Vec<(&str, String)> = Vec::new();
        match device {
            None => {
                fields.push(("NAME", "LOCAL AUDIO".to_string()));
                fields.push(("ENGINE", kit::upper(&self.local_backend())));
            }
            Some(dev) => {
                fields.push(("NAME", kit::upper(&dev.friendly_name)));
                if !dev.manufacturer.is_empty() {
                    fields.push(("MAKER", kit::upper(&dev.manufacturer)));
                }
                if !dev.model_name.is_empty() {
                    fields.push(("MODEL", kit::upper(&dev.model_name)));
                }
                fields.push(("HOST", dev.host.clone()));
                fields.push(("PORT", dev.port.to_string()));
                // Ids and paths keep their case (resolution 2).
                if !dev.udn.is_empty() {
                    fields.push(("UDN", middle_cut(&dev.udn, value_cols)));
                }
                if !dev.av_transport_control_url.is_empty() {
                    fields.push(("CONTROL", url_path(&dev.av_transport_control_url)));
                }
            }
        }
        let status = if is_active { "ACTIVE" } else { "NOT IN USE" };
        fields.push(("STATUS", status.to_string()));
        if is_active {
            fields.push(("VOLUME", self.player.volume().to_string()));
        }

        let bold = theme::ink().add_modifier(Modifier::BOLD);
        let mut dy = 0;
        for (key, value) in &fields {
            if dy >= list.height {
                return;
            }
            let style = match *key {
                "STATUS" if is_active => bold,
                "STATUS" => theme::mid(),
                _ => theme::ink(),
            };
            let cols = [
                Col::fixed(*key, KEY_COLS, theme::dim()),
                Col::flex(value.as_str(), style),
            ];
            kit::list_row(f, Rect::new(list.x, list.y + dy, list.width, 1), &cols, RowMark::Plain);
            dy += 1;
        }
        // Enter on this row makes it the output (2d); the active one already is.
        if !is_active && dy + 1 < list.height {
            let line = Line::from(vec![
                Span::styled(" ENTER ", theme::keycap()),
                Span::styled(" SWITCH OUTPUT HERE", theme::mid()),
            ]);
            let row = Rect::new(list.x, list.y + dy + 1, list.width, 1).intersection(f.area());
            if !row.is_empty() {
                f.render_widget(line, row);
            }
        }
    }
}
