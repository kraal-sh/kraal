//! `kraal config reload`: read config.toml again while the server runs.

use super::{Server, events, frame_bytes};
use crate::config::Config;
use crate::keymap::Keymap;

impl Server {
    /// Read the configuration again and apply it: keys, theme, sidebar, bottom bar (when its
    /// setting changed: a toggle for the session holds until then), mouse, pane frames,
    /// notifications, tools, hints, hooks, production words, contexts, saving scrollback and the
    /// paste buffers' limits and file. Panes
    /// already open keep their shell and scrollback length; new panes take the new ones. When the
    /// file cannot be read, nothing changes. Returns the warnings.
    pub(super) fn reload_config(&mut self) -> Result<Vec<String>, String> {
        let config = Config::load().map_err(|e| format!("config: {e:#}"))?;
        let mut warnings = Vec::new();
        let (mut keymap, w) = Keymap::build(&config.keys);
        warnings.extend(w);
        warnings.extend(keymap.bind_tools(&config.tools));
        warnings.extend(crate::tools::check(&config.tools));
        let (hint_patterns, w) = crate::hints::Custom::build(&config.hints, &config.tools);
        warnings.extend(w);
        let (hook_list, w) = events::read_hooks(&config.hooks);
        warnings.extend(w);
        let (theme, w) = crate::theme::Theme::from_config(&config.theme);
        warnings.extend(w);
        crate::theme::set(theme);
        self.keymap = keymap;
        self.hint_patterns = hint_patterns;
        // Hooks that are running keep their supervision; the new list applies to the next events.
        self.hooks.list = hook_list;
        self.env.shell = config.shell();
        self.env.scrollback = config.scrollback;
        // Attached terminals report the mouse, or stop, as the new setting says.
        if config.mouse != self.config.mouse {
            let modes = if config.mouse { crate::client::MOUSE_ON } else { crate::client::MOUSE_OFF };
            for c in self.clients.values().filter(|c| c.attached) {
                let _ = c.out.try_send(frame_bytes(modes.as_bytes()));
            }
        }
        if config.bar.keys != self.config.bar.keys {
            self.bottom_bar = config.bar.keys != crate::config::BarKeys::Off;
        }
        let persisted = self.config.buffers.persist;
        self.config = config;
        self.reconfigure_buffers(persisted);
        // Workspace folders, the sidebar's width, the bottom bar and pane frames may have changed.
        self.scan_definitions();
        self.relayout();
        self.mark_dirty();
        Ok(warnings)
    }
}
