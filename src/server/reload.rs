//! `kraal config reload`: read config.toml again while the server runs.

use super::{Server, events};
use crate::config::Config;
use crate::keymap::Keymap;

impl Server {
    /// Read the configuration again and apply it: keys, theme, sidebar, mouse, pane frames,
    /// notifications, tools, hints, hooks, production words and contexts. Panes already open keep
    /// their shell and scrollback length; new panes take the new ones. When the file cannot be
    /// read, nothing changes. Returns the warnings.
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
        self.hooks = events::Hooks::new(hook_list);
        self.env.shell = config.shell();
        self.env.scrollback = config.scrollback;
        self.config = config;
        // Workspace folders, the sidebar's width and pane frames may have changed.
        self.scan_definitions();
        self.relayout();
        self.mark_dirty();
        Ok(warnings)
    }
}
