//! The bottom bar: on the last row, across the whole width, what the focused pane is and what can
//! be done there, each action with the keys the keymap in use gives it, the most useful first;
//! the menu key at the right end.

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::overlays::shown_key;
use super::render::{key_list, spans_width};
use super::ui::Mode;
use super::{Client, ClientId, Server};
use crate::agent::AgentState;
use crate::keymap::{Action, Keymap};
use crate::pane::Content;

/// What the focused pane is, as far as the bar is concerned.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Context {
    /// Connected to a server.
    Server { name: String, production: bool },
    /// A coding agent: its pane's name, its state, whether kraal started it on an account (which
    /// can then be switched), whether it works in a worktree.
    Agent { name: String, state: AgentState, account: bool, worktree: bool },
    /// A shell, or another program, in a git repository.
    Repository,
    /// A shell or another program; also what the bar offers without a pane.
    Shell,
    /// A file's preview, which reads its own keys.
    Preview,
    /// The file manager, whose own footer shows its keys.
    Files,
}

/// How an action the bar offers is reached.
enum Reach {
    /// By the keys the keymap binds to the action; a click runs it.
    Bound(Action),
    /// By a key the pane reads itself (a preview's): a click does not press it.
    PaneKey(&'static str),
}

impl Context {
    /// What the bar says of the pane before its actions: the server, or the agent and its state.
    fn lead(&self) -> Option<Vec<Span<'static>>> {
        let t = crate::theme::current();
        let (name, color, state) = match self {
            Context::Server { name, production } => (name, if *production { t.production } else { t.text }, None),
            Context::Agent { name, state, .. } => (name, t.state_color(*state), Some(state.label())),
            _ => return None,
        };
        let mut out = vec![Span::styled(
            crate::text::truncate(name, LEAD_NAME_MAX),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if let Some(state) = state {
            out.push(Span::styled(format!(" {state}"), Style::default().fg(color)));
        }
        out.push(Span::styled(format!(" {}", t.pointer()), Style::default().fg(t.dim)));
        Some(out)
    }

    /// The actions offered, the most useful first.
    fn offers(&self) -> Vec<(Reach, &'static str)> {
        use Reach::{Bound, PaneKey};
        match self {
            Context::Server { .. } => vec![
                (Bound(Action::Files), "its files"),
                (Bound(Action::ToolPicker), "tools there"),
                (Bound(Action::Download), "download"),
                (Bound(Action::Upload), "upload"),
            ],
            Context::Agent { account, worktree, .. } => {
                let mut out = Vec::new();
                if *account {
                    out.push((Bound(Action::SwitchAccount), "switch account"));
                }
                if *worktree {
                    out.push((Bound(Action::GitActions), "review its worktree"));
                }
                out.extend([(Bound(Action::NextAttention), "next agent"), (Bound(Action::AgentPicker), "new agent")]);
                out
            }
            Context::Repository => vec![
                (Bound(Action::GitActions), "git and worktrees"),
                (Bound(Action::ToolPicker), "tools"),
                (Bound(Action::NewPane), "new pane"),
                (Bound(Action::SplitRight), "split"),
                (Bound(Action::AgentPicker), "start an agent"),
            ],
            Context::Shell => vec![
                (Bound(Action::NewPane), "new pane"),
                (Bound(Action::SplitRight), "split"),
                (Bound(Action::SshPicker), "connect to a server"),
                (Bound(Action::AgentPicker), "start an agent"),
            ],
            Context::Preview => vec![(PaneKey("↑↓"), "scroll"), (PaneKey("r"), "reload"), (PaneKey("q"), "close")],
            Context::Files => vec![(Bound(Action::NewPane), "new pane"), (Bound(Action::SplitRight), "split")],
        }
    }
}

/// Columns a pane's name takes at most at the start of the bar.
const LEAD_NAME_MAX: usize = 24;
/// Columns between two actions.
const GAP: u16 = 2;

/// What a click on the bar does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Click {
    Run(Action),
    /// Open the menu, as the prefix key does.
    Menu,
}

/// An entry of the bar: column, width, what a click on it does, its text.
type Entry = (u16, u16, Option<Click>, Vec<Span<'static>>);

/// What the bottom bar shows, from the left: the lead, the actions that fit whole, the menu key
/// at the right end.
#[derive(Debug, Default, PartialEq)]
pub(super) struct BottomBar {
    entries: Vec<Entry>,
}

impl BottomBar {
    /// The bar of a pane of `context`, `width` columns wide, with the keys of `keymap`; without
    /// `keys` (in a mode that reads keys itself, where they would not reach the actions), only
    /// what the pane is.
    pub(super) fn new(context: &Context, keymap: &Keymap, width: u16, keys: bool) -> BottomBar {
        let ascii = crate::theme::current().ascii;
        let menu = keys.then(|| key_list(&[(&keymap.prefix_label(), "menu")]));
        let menu_w = menu.as_ref().map_or(0, |m| spans_width(m));
        // A blank column at each end, and a gap before the menu.
        let end = width.saturating_sub(1);
        let limit = if menu.is_some() { end.saturating_sub(menu_w + GAP) } else { end };
        let offers = if keys { context.offers() } else { Vec::new() };
        let actions = offers.into_iter().filter_map(|(reach, label)| {
            let (key, click) = match reach {
                Reach::Bound(a) => (keymap.shown_hint(a, ascii)?, Some(Click::Run(a))),
                Reach::PaneKey(k) => (shown_key(k), None),
            };
            Some((key_list(&[(&key, label)]), click, GAP))
        });
        // The lead's pointer goes right before the first action.
        let pieces = context.lead().map(|lead| (lead, None, 1)).into_iter().chain(actions);
        let mut entries = Vec::new();
        let mut x = 1;
        for (spans, click, gap) in pieces {
            let w = spans_width(&spans);
            if x + w > limit {
                break;
            }
            entries.push((x, w, click, spans));
            x += w + gap;
        }
        if let Some(menu) = menu.filter(|_| menu_w < end) {
            entries.push((end - menu_w, menu_w, Some(Click::Menu), menu));
        }
        BottomBar { entries }
    }

    /// What a click at column `col` does.
    pub(super) fn hit(&self, col: u16) -> Option<Click> {
        self.entries.iter().find(|(x, w, ..)| (*x..x + w).contains(&col)).and_then(|(_, _, click, _)| *click)
    }

    /// Draw it on row `y`.
    pub(super) fn draw(&self, y: u16, buf: &mut Buffer) {
        for (x, w, _, spans) in &self.entries {
            buf.set_line(*x, y, &Line::from(spans.clone()), *w);
        }
    }
}

impl Server {
    /// Client `c`'s bottom bar.
    pub(super) fn bottom_bar(&self, c: &Client) -> BottomBar {
        BottomBar::new(&self.bar_context(c), &self.keymap, c.cols, matches!(c.mode, Mode::Normal | Mode::Leader { .. }))
    }

    /// What a click at column `col` of client `id`'s bottom bar does.
    pub(super) fn bottom_bar_hit(&self, id: ClientId, col: u16) -> Option<Click> {
        self.bottom_bar(self.clients.get(&id)?).hit(col)
    }

    /// What client `c`'s focused pane is. An agent counts before its server, a server before a
    /// repository.
    fn bar_context(&self, c: &Client) -> Context {
        let Some((id, pane)) = self.client_focus(c.id).and_then(|id| Some((id, self.panes.get(&id)?))) else {
            return Context::Shell;
        };
        match pane.content {
            Content::Preview(_) => return Context::Preview,
            Content::Files(_) => return Context::Files,
            Content::Term(_) => {}
        }
        if pane.agent.kind.is_some() {
            return Context::Agent {
                name: pane.display_name(),
                state: pane.agent.state,
                account: self.pane_account(id).is_some(),
                worktree: self.pane_worktree(id).is_some(),
            };
        }
        if let Some(server) = self.pane_server(id) {
            return Context::Server { name: server.name, production: self.pane_production_shown(id).is_some() };
        }
        if self.pane_git(id).is_some() { Context::Repository } else { Context::Shell }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap(user: &str) -> Keymap {
        let (km, warnings) = Keymap::build(&crate::config::Config::from_user_toml(user).unwrap().keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        km
    }

    /// The bar as a row of text.
    fn text(bar: &BottomBar, width: u16) -> String {
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, width, 1));
        bar.draw(0, &mut buf);
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    fn shown(context: &Context, km: &Keymap) -> String {
        text(&BottomBar::new(context, km, 160, true), 160).trim().to_string()
    }

    fn server(production: bool) -> Context {
        Context::Server { name: "web-prod-1".into(), production }
    }

    fn agent(account: bool, worktree: bool) -> Context {
        Context::Agent { name: "api".into(), state: AgentState::Blocked, account, worktree }
    }

    #[test]
    fn each_kind_of_pane_has_its_actions() {
        let km = keymap("");
        let want = [
            (
                server(true),
                "web-prod-1 ▸ ctrl+b f its files  ctrl+b r tools there  ctrl+b S d download  ctrl+b S u upload",
            ),
            (
                agent(true, true),
                "api blocked ▸ ctrl+b A c switch account  ctrl+b R g review its worktree  alt+a next agent  \
                 ctrl+b a new agent",
            ),
            (agent(false, false), "api blocked ▸ alt+a next agent  ctrl+b a new agent"),
            (
                Context::Repository,
                "ctrl+b R g git and worktrees  ctrl+b r tools  alt+n new pane  ctrl+b P → split  \
                 ctrl+b a start an agent",
            ),
            (Context::Shell, "alt+n new pane  ctrl+b P → split  ctrl+b s connect to a server  ctrl+b a start an agent"),
            (Context::Preview, "↑↓ scroll  r reload  q close"),
            (Context::Files, "alt+n new pane  ctrl+b P → split"),
        ];
        for (context, actions) in want {
            let bar = shown(&context, &km);
            assert!(bar.starts_with(actions) && bar.ends_with("ctrl+b menu"), "{context:?}: {bar:?}");
        }
    }

    #[test]
    fn a_production_server_is_named_in_red() {
        let km = keymap("");
        let color = |production| BottomBar::new(&server(production), &km, 120, true).entries[0].3[0].style.fg;
        assert_eq!(color(true), Some(crate::theme::current().production));
        assert_eq!(color(false), Some(crate::theme::current().text));
    }

    #[test]
    fn the_keys_are_those_of_the_keymap() {
        let km = keymap("[keys]\nprefix = [\"ctrl+a\"]\n[keys.leader]\ndownload = []\n[keys.direct]\nnew_pane = []\n");
        let bar = shown(&server(false), &km);
        assert!(bar.starts_with("web-prod-1 ▸ ctrl+a f its files  ctrl+a r tools there  ctrl+a S u upload"), "{bar}");
        assert!(bar.ends_with("ctrl+a menu"), "{bar}");
        assert!(shown(&Context::Shell, &km).starts_with("ctrl+a n new pane"));
        let classic = keymap("[keys]\npreset = \"classic\"\n");
        let bar = shown(&server(false), &classic);
        assert!(bar.starts_with("web-prod-1 ▸ ctrl+b f its files  ctrl+b T tools there  ctrl+b D download"), "{bar}");
        assert!(shown(&Context::Shell, &classic).starts_with("alt+n new pane  ctrl+b % split"));
    }

    #[test]
    fn a_narrow_bar_drops_whole_actions_from_the_right_and_keeps_the_menu() {
        let km = keymap("");
        let whole = BottomBar::new(&server(true), &km, 200, true);
        let row = text(&whole, 200);
        // Where a narrower bar may stop: before an entry.
        let cuts: Vec<String> =
            whole.entries.iter().map(|(x, ..)| row.chars().take(usize::from(*x)).collect::<String>()).collect();
        let menu = "ctrl+b menu ";
        for width in 13..=120 {
            let row = text(&BottomBar::new(&server(true), &km, width, true), width);
            let (left, right) = row.split_at(row.len() - menu.len());
            assert_eq!(right, menu, "{width}");
            assert!(cuts.iter().any(|cut| cut.trim_end() == left.trim_end()), "{width}: {row:?}");
            assert!(left.trim().is_empty() || left.ends_with("  "), "{width}: {row:?}");
        }
        for width in 0..13 {
            assert!(BottomBar::new(&server(true), &km, width, true).entries.is_empty(), "{width}");
        }
        let row = text(&BottomBar::new(&server(true), &km, 40, true), 40);
        assert_eq!(row, format!(" web-prod-1 ▸{}{menu}", " ".repeat(15)));
    }

    #[test]
    fn a_click_runs_the_action_under_it() {
        let km = keymap("");
        // The column where `what` starts in `bar`.
        let at = |bar: &BottomBar, what: &str| {
            let row = text(bar, 120);
            row[..row.find(what).unwrap()].chars().count() as u16
        };
        let bar = BottomBar::new(&server(false), &km, 120, true);
        assert_eq!(bar.hit(at(&bar, "web-prod-1")), None);
        assert_eq!(bar.hit(at(&bar, "ctrl+b f")), Some(Click::Run(Action::Files)));
        assert_eq!(bar.hit(at(&bar, "ctrl+b f") - 1), None);
        assert_eq!(bar.hit(at(&bar, "tools there") + 3), Some(Click::Run(Action::ToolPicker)));
        assert_eq!(bar.hit(at(&bar, "menu")), Some(Click::Menu));
        let preview = BottomBar::new(&Context::Preview, &km, 120, true);
        assert_eq!(preview.hit(at(&preview, "reload")), None, "a key of the pane itself");
    }

    #[test]
    fn other_modes_show_what_the_pane_is_without_keys() {
        let km = keymap("");
        assert_eq!(text(&BottomBar::new(&agent(true, true), &km, 60, false), 60).trim_end(), " api blocked ▸");
        assert!(BottomBar::new(&Context::Shell, &km, 60, false).entries.is_empty());
    }
}
