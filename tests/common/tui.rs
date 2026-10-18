//! Rendering helpers for the TUI tests: draw an `AppUI` into a test buffer, read its rows as
//! text, find text by content (not by row number) and read cell styles.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::Terminal;

use quickstream::ui::AppUI;

/// Draw one frame of `app` at `width` x `height` and return the buffer.
pub fn render(app: &mut AppUI, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().buffer().clone()
}

/// Draw one frame and return it as text, one line per row.
pub fn render_text(app: &mut AppUI, width: u16, height: u16) -> String {
    rows(&render(app, width, height)).join("\n")
}

/// The symbols of row `y`, one string per cell.
pub fn cells(buffer: &Buffer, y: u16) -> Vec<String> {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol().to_string())
        .collect()
}

/// Row `y` as text (the cells' symbols joined; quotes are not escaped, unlike the backend's
/// `to_string`).
pub fn row(buffer: &Buffer, y: u16) -> String {
    cells(buffer, y).concat()
}

/// Every row as text.
pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height).map(|y| row(buffer, y)).collect()
}

/// Index of the first row containing `text`.
pub fn find_row(buffer: &Buffer, text: &str) -> Option<u16> {
    (0..buffer.area.height).find(|&y| row(buffer, y).contains(text))
}

/// Cell position `(x, y)` where `text` starts, searching rows top to bottom. Matches whole cell
/// symbols, so `x` is a column even when the row holds multi-byte characters.
pub fn find_text(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
    let wanted: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    let len = wanted.len();
    for y in 0..buffer.area.height {
        let row = cells(buffer, y);
        if row.len() < len {
            continue;
        }
        if let Some(x) = (0..=row.len() - len).find(|&x| row[x..x + len] == wanted[..]) {
            return Some((x as u16, y));
        }
    }
    None
}

/// Style of the cell at `(x, y)`.
pub fn cell_style(buffer: &Buffer, x: u16, y: u16) -> Style {
    buffer[(x, y)].style()
}

/// Styles of the cells under `text` (where [`find_text`] finds it), or `None` if it is absent.
pub fn text_styles(buffer: &Buffer, text: &str) -> Option<Vec<Style>> {
    let (x, y) = find_text(buffer, text)?;
    Some(
        (x..x + text.chars().count() as u16)
            .map(|cx| cell_style(buffer, cx, y))
            .collect(),
    )
}
