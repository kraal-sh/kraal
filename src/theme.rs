//! Colours and symbols of kraal's own interface (frames, bars, icons). Pane contents keep the
//! programs' colours; kraal uses the terminal's palette by default so it follows your theme.

use std::str::FromStr;
use std::sync::RwLock;

use ratatui::style::Color;
use ratatui::symbols::border;
use serde::Deserialize;

use crate::agent::AgentState;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeConfig {
    /// "default", "high-contrast" or "ascii".
    #[serde(default)]
    pub preset: Option<String>,
    pub accent: Option<String>,
    pub dim: Option<String>,
    pub sync: Option<String>,
    pub production: Option<String>,
    /// "rounded", "plain", "thick", "double" or "ascii".
    pub frames: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub accent: Color,
    /// Secondary text, unfocused frames.
    pub dim: Color,
    /// Normal interface text (tab names, details).
    pub text: Color,
    pub sync: Color,
    /// Frames and badges of production panes.
    pub production: Color,
    pub working: Color,
    pub blocked: Color,
    pub done: Color,
    pub idle: Color,
    /// Background of the selected row of a list.
    pub selection: Color,
    pub frames: Frames,
    /// Only ASCII in the interface: no box drawing, no symbols.
    pub ascii: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frames {
    Rounded,
    Plain,
    Thick,
    Double,
    Ascii,
}

const ASCII_SET: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

const ASCII_FOCUS_SET: border::Set<'static> = border::Set {
    top_left: "#",
    top_right: "#",
    bottom_left: "#",
    bottom_right: "#",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "=",
    horizontal_bottom: "=",
};

impl Theme {
    pub const DEFAULT: Theme = Theme {
        accent: Color::Green,
        dim: Color::DarkGray,
        text: Color::Gray,
        sync: Color::Magenta,
        production: Color::Red,
        working: Color::Yellow,
        blocked: Color::LightRed,
        done: Color::Green,
        idle: Color::Gray,
        selection: Color::Indexed(237),
        frames: Frames::Rounded,
        ascii: false,
    };

    /// Brighter, bolder choices for low-contrast screens or eyes.
    pub const HIGH_CONTRAST: Theme = Theme {
        accent: Color::LightYellow,
        dim: Color::Gray,
        text: Color::White,
        sync: Color::LightMagenta,
        production: Color::LightRed,
        working: Color::LightCyan,
        blocked: Color::LightRed,
        done: Color::LightGreen,
        idle: Color::White,
        selection: Color::Blue,
        frames: Frames::Plain,
        ascii: false,
    };

    pub const ASCII: Theme = Theme { frames: Frames::Ascii, ascii: true, ..Theme::DEFAULT };

    /// Resolve a config section; returns warnings for values that could not be read.
    pub fn from_config(cfg: &ThemeConfig) -> (Theme, Vec<String>) {
        let mut warnings = Vec::new();
        let mut t = match cfg.preset.as_deref().unwrap_or("default") {
            "default" => Theme::DEFAULT,
            "high-contrast" => Theme::HIGH_CONTRAST,
            "ascii" => Theme::ASCII,
            other => {
                warnings.push(format!("theme.preset: unknown preset {other:?} (default, high-contrast, ascii)"));
                Theme::DEFAULT
            }
        };
        let mut color = |name: &str, value: &Option<String>, slot: &mut Color| {
            if let Some(v) = value {
                match Color::from_str(v) {
                    Ok(c) => *slot = c,
                    Err(_) => warnings.push(format!("theme.{name}: {v:?} is not a colour (a name, #rrggbb or 0-255)")),
                }
            }
        };
        color("accent", &cfg.accent, &mut t.accent);
        color("dim", &cfg.dim, &mut t.dim);
        color("sync", &cfg.sync, &mut t.sync);
        color("production", &cfg.production, &mut t.production);
        if let Some(f) = &cfg.frames {
            t.frames = match f.as_str() {
                "rounded" => Frames::Rounded,
                "plain" => Frames::Plain,
                "thick" => Frames::Thick,
                "double" => Frames::Double,
                "ascii" => Frames::Ascii,
                other => {
                    warnings.push(format!("theme.frames: unknown style {other:?}"));
                    t.frames
                }
            };
        }
        (t, warnings)
    }

    pub fn state_color(&self, s: AgentState) -> Color {
        match s {
            AgentState::Working => self.working,
            AgentState::Blocked => self.blocked,
            AgentState::Done => self.done,
            AgentState::Idle => self.idle,
            AgentState::Unknown => self.dim,
        }
    }

    /// The mark of an agent state: a dot in its colour, hollow when idle.
    pub fn icon(&self, s: AgentState) -> &'static str {
        if !self.ascii {
            return match s {
                AgentState::Unknown => "·",
                AgentState::Idle => "○",
                AgentState::Working | AgentState::Blocked | AgentState::Done => "●",
            };
        }
        match s {
            AgentState::Unknown => ".",
            AgentState::Idle => "o",
            AgentState::Working => "*",
            AgentState::Blocked => "!",
            AgentState::Done => "+",
        }
    }

    /// Border symbols for a pane frame or an overlay.
    pub fn border(&self, focused: bool) -> border::Set<'static> {
        match (self.frames, focused) {
            (Frames::Ascii, true) => ASCII_FOCUS_SET,
            (Frames::Ascii, false) => ASCII_SET,
            // The focused pane stands out with heavier lines.
            (Frames::Rounded, true) | (Frames::Plain, true) | (Frames::Thick, _) => border::THICK,
            (Frames::Rounded, false) => border::ROUNDED,
            (Frames::Plain, false) => border::PLAIN,
            (Frames::Double, _) => border::DOUBLE,
        }
    }

    /// Overlays (pickers, prompts) use the unfocused style.
    pub fn overlay_border(&self) -> border::Set<'static> {
        self.border(false)
    }

    /// Where a line across a box meets its left and right sides.
    pub fn tees(&self) -> (&'static str, &'static str) {
        match self.frames {
            Frames::Ascii => ("+", "+"),
            Frames::Thick => ("┣", "┫"),
            Frames::Double => ("╠", "╣"),
            Frames::Rounded | Frames::Plain => ("├", "┤"),
        }
    }

    pub fn hline(&self) -> &'static str {
        if self.ascii { "-" } else { "─" }
    }

    pub fn bar(&self, pct: f64) -> String {
        let filled = ((pct / 100.0) * 8.0).round().clamp(0.0, 8.0) as usize;
        let (on, off) = if self.ascii { ("#", "-") } else { ("▓", "░") };
        format!("{}{}", on.repeat(filled), off.repeat(8 - filled))
    }

    /// Marker of the current item in lists.
    pub fn pointer(&self) -> &'static str {
        if self.ascii { ">" } else { "▸" }
    }
}

static CURRENT: RwLock<Theme> = RwLock::new(Theme::DEFAULT);

/// The interface theme in use.
pub fn current() -> Theme {
    CURRENT.read().map(|t| *t).unwrap_or(Theme::DEFAULT)
}

pub fn set(t: Theme) {
    if let Ok(mut c) = CURRENT.write() {
        *c = t;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_and_overrides() {
        let (t, w) = Theme::from_config(&ThemeConfig {
            preset: Some("ascii".into()),
            accent: Some("#ff8800".into()),
            ..Default::default()
        });
        assert!(w.is_empty(), "{w:?}");
        assert!(t.ascii);
        assert_eq!(t.accent, Color::Rgb(0xff, 0x88, 0x00));
        assert_eq!(t.icon(AgentState::Blocked), "!");
        assert_eq!(t.bar(50.0), "####----");
        assert_eq!(t.border(true).top_left, "#");
        let (t, w) = Theme::from_config(&ThemeConfig {
            accent: Some("12".into()),
            frames: Some("double".into()),
            ..Default::default()
        });
        assert!(w.is_empty());
        assert_eq!(t.accent, Color::Indexed(12));
        assert_eq!(t.border(false).top_left, border::DOUBLE.top_left);
        let (_, w) = Theme::from_config(&ThemeConfig {
            preset: Some("neon".into()),
            dim: Some("not a colour".into()),
            ..Default::default()
        });
        assert_eq!(w.len(), 2);
    }
}
