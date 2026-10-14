//! Commands the shells mark (OSC 133): a long one that ends out of sight is announced, and the
//! last one's output can be copied.

use std::time::Duration;

use super::{ClientId, Server};
use crate::layout::PaneId;
use crate::pane::{Finished, short_duration};
use crate::theme::NoticeKind;

/// What to say where the shell marks no commands.
pub(super) const NO_MARKS: &str =
    "no command marked in this pane: `kraal integration install zsh` (or bash, fish) marks them";

impl Server {
    /// A marked command ended in `pane`.
    pub(super) fn command_done(&mut self, pane: PaneId, done: Finished) {
        let fields = serde_json::json!({
            "command": done.text,
            "exit": done.exit,
            "seconds": done.duration.as_secs_f64(),
        });
        self.emit_pane("command.finished", pane, fields);
        self.mark_dirty_pane(pane);
        // It may have changed what git says there (a commit, a checkout, an edit).
        if let Some(dir) = self.panes.get(&pane).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd()) {
            self.git_soon(&dir);
        }
        if !announced(self.config.notifications.long_command, done.duration, self.pane_watched(pane)) {
            return;
        }
        if let Some(p) = self.panes.get_mut(&pane) {
            p.set_command_unseen(true);
        }
        let what = done.text.unwrap_or_else(|| "command".into());
        let how = match done.exit {
            Some(0) | None => "done".to_string(),
            Some(code) => format!("failed ({code})"),
        };
        let body = format!("{what}: {how} after {}", short_duration(done.duration));
        let title = self.pane_label(pane);
        self.notify(
            if done.exit.is_some_and(|code| code != 0) { NoticeKind::Error } else { NoticeKind::Notice },
            &title,
            &body,
            Some(pane),
        );
    }

    /// Copy the output of the focused pane's last command (the one running, or the last that
    /// ran).
    pub(super) fn copy_last_output(&mut self, id: ClientId, focus: Option<PaneId>) {
        let found = focus.and_then(|p| self.panes.get_mut(&p)).and_then(|p| p.last_command_output());
        match found {
            Some((_, output)) if output.is_empty() => {
                self.toast(id, NoticeKind::Notice, "the last command printed nothing")
            }
            Some((_, output)) => self.copy_to_client(id, &output),
            None => self.toast(id, NoticeKind::Notice, NO_MARKS),
        }
    }
}

/// Whether a command that ran for `ran` is announced: long enough (`threshold` seconds, 0 for
/// never), and ended where nobody was looking.
fn announced(threshold: u64, ran: Duration, watched: bool) -> bool {
    threshold > 0 && ran >= Duration::from_secs(threshold) && !watched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copying_command_output_keeps_the_terminal_delivery_message() {
        use crate::pane::{Pane, PaneSpec};
        use crate::server::{
            queue,
            tests::{client, server},
        };

        let (tx, _events) = queue::channel(4096);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let spec = PaneSpec { command: vec!["true".into()], ..Default::default() };
        let mut pane = Pane::spawn(99, None, spec, 80, 24, &server.env, &server.tx).unwrap();
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hello\r\n\x1b]133;C\x07hello\r\n\x1b]133;D;0\x07");
        server.panes.insert(99, pane);
        server.copy_last_output(id, Some(99));
        assert_eq!(server.buffers.get(None).unwrap().text, "hello");
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "copy sent to terminal");
    }

    #[test]
    fn only_long_commands_ending_out_of_sight_are_announced() {
        let secs = Duration::from_secs;
        assert!(announced(30, secs(30), false));
        assert!(announced(30, secs(600), false));
        assert!(!announced(30, secs(29), false), "short");
        assert!(!announced(30, secs(600), true), "someone was looking");
        assert!(!announced(0, secs(600), false), "turned off");
    }
}
