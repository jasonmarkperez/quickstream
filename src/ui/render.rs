//! `AppUI::render`: the chrome, the active tab's body and the open modal over the dimmed screen.

use ratatui::prelude::*;

use super::{chrome, help, kit, mix_picker, screen, search_view, theme, AppUI};

impl AppUI {
    pub fn render(&mut self, f: &mut Frame) {
        // Every cell starts as INK on GROUND; widgets drawn later keep that background unless
        // they set their own.
        let area = f.area();
        f.buffer_mut().set_style(area, theme::base());
        let screen = screen::Screen::new(area);
        let layout = chrome::ChromeLayout::new(&screen, self.active_tab != 0);

        if let Some(header) = layout.header {
            self.render_header(f, header);
        }
        self.render_tab_row(f, layout.tabs, &screen, layout.header.is_none());

        let content = layout.content;
        if content.width > 0 && content.height > 0 {
            match self.active_tab {
                0 => self.render_now_playing_tab(f, content),
                1 => self.render_queue_tab(f, content),
                2 => self.render_devices_tab(f, content),
                3 => self.render_settings_tab(f, content),
                4 => self.render_playlists_tab(f, content),
                _ => {}
            }
        }

        if let Some(rule) = layout.rule {
            self.render_rule(f, rule);
        }
        if let Some(strip) = layout.strip {
            self.render_strip(f, strip, &screen);
        }

        // A modal is drawn over the screen dimmed to RULE on GROUND, except the key row, drawn
        // after the dimming: it shows the modal's keys and the status message (search's `Tab`
        // says what it added only there).
        if self.modal_open() {
            kit::dim_buffer(f.buffer_mut(), area);
        }
        self.render_key_row(f, layout.keys, &screen);
        if let Some(ref mut modal) = self.search_modal {
            search_view::render(f, modal, self.queue_tab.home.as_deref());
        } else if self.playlists.modal.is_some() {
            self.render_playlist_modal(f);
        } else if self.show_mix_picker_modal {
            self.render_mix_picker_modal(f);
        } else if self.show_help_modal {
            self.render_help_modal(f);
        }
    }

    /// Whether a modal (search, playlist prompt or picker, mix picker, help) is open.
    fn modal_open(&self) -> bool {
        self.search_modal.is_some()
            || self.playlists.modal.is_some()
            || self.show_mix_picker_modal
            || self.show_help_modal
    }

    fn render_help_modal(&self, f: &mut Frame) {
        let screen = f.area();
        help::render(f, screen, self.settings_mgr.settings.seek_step_seconds);
    }

    fn render_mix_picker_modal(&mut self, f: &mut Frame) {
        mix_picker::render(
            f,
            self.mix_picker_cursor,
            &self.active_mix_profile,
            &mut self.mix_picker_list,
        );
    }
}
