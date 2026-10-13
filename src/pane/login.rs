//! A login ends with a check, or an explicit choice to try again. Enter always closes it.

use crate::account_status::Status;
use crate::accounts::Account;
use crate::input::{Key, KeyCode};

#[derive(Debug, Clone)]
pub struct Login {
    pub account: Account,
    pub state: LoginState,
    pub lock: Option<std::sync::Arc<std::fs::File>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    Running,
    Checking,
    Failed,
    Cancelled,
    Checked(Status),
    SaveFailed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginAction {
    Close,
    Retry,
    Check,
}

impl LoginState {
    pub fn exited(self, code: Option<i32>) -> Self {
        if self != Self::Running {
            return self;
        }
        match code {
            Some(0) => Self::Checking,
            Some(130 | 143) | None => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Running => "signing in",
            Self::Checking => "checking sign-in",
            Self::Failed => "sign-in failed",
            Self::Cancelled => "sign-in cancelled",
            Self::Checked(status) => status.label(),
            Self::SaveFailed => "signed in, could not save account",
        }
    }

    pub fn actions(self) -> &'static str {
        match self {
            Self::Running => "ctrl+c cancel",
            Self::Checking | Self::Checked(Status::SignedIn) => "enter close",
            Self::Failed | Self::Cancelled => "enter close · r try again",
            Self::Checked(_) => "enter close · c check again · r try again",
            Self::SaveFailed => "enter close · c check again",
        }
    }

    pub fn action(self, key: &Key) -> Option<LoginAction> {
        match (key.code, key.mods.ctrl) {
            (KeyCode::Enter | KeyCode::Esc, _) | (KeyCode::Char('x' | 'q'), false) | (KeyCode::Char('c'), true) => {
                Some(LoginAction::Close)
            }
            (KeyCode::Char('r'), false)
                if !matches!(
                    self,
                    Self::Running | Self::Checking | Self::SaveFailed | Self::Checked(Status::SignedIn)
                ) =>
            {
                Some(LoginAction::Retry)
            }
            (KeyCode::Char('c'), false)
                if matches!(self, Self::Checked(_) | Self::SaveFailed) && self != Self::Checked(Status::SignedIn) =>
            {
                Some(LoginAction::Check)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    #[test]
    fn an_exit_checks_success_and_keeps_failure_or_cancellation() {
        for (code, state) in [
            (Some(0), LoginState::Checking),
            (Some(1), LoginState::Failed),
            (Some(130), LoginState::Cancelled),
            (Some(143), LoginState::Cancelled),
            (None, LoginState::Cancelled),
        ] {
            assert_eq!(LoginState::Running.exited(code), state);
        }
        assert_eq!(LoginState::Cancelled.exited(Some(0)), LoginState::Cancelled);
        assert_eq!(LoginState::Failed.exited(Some(0)), LoginState::Failed);
    }

    #[test]
    fn enter_closes_and_retry_or_check_is_always_explicit() {
        let key = |code| Key { code, mods: Mods::NONE, raw: Vec::new() };
        for state in [
            LoginState::Checking,
            LoginState::Failed,
            LoginState::Cancelled,
            LoginState::Checked(Status::NotInstalled),
            LoginState::Checked(Status::NotSignedIn),
            LoginState::Checked(Status::CouldNotCheck),
            LoginState::Checked(Status::SignedIn),
            LoginState::SaveFailed,
        ] {
            assert_eq!(state.action(&key(KeyCode::Enter)), Some(LoginAction::Close));
            assert_eq!(state.action(&key(KeyCode::Esc)), Some(LoginAction::Close));
            assert_eq!(
                state.action(&key(KeyCode::Char('r'))) == Some(LoginAction::Retry),
                state.actions().contains("r try again")
            );
            assert_eq!(
                state.action(&key(KeyCode::Char('c'))) == Some(LoginAction::Check),
                state.actions().contains("c check again")
            );
        }
    }

    #[test]
    fn login_output_stays_in_the_terminal_only() {
        let mut pane = super::super::tests::fed_pane(80, 24, 100);
        pane.spec.login = Some(Login {
            account: Account {
                id: crate::accounts::new_uuid(),
                name: "work".into(),
                agent: "claude".into(),
                home: "/tmp/work".into(),
                threshold: None,
                note: None,
                sign_in_required: true,
                review: false,
                public_identity: None,
                linked_to: None,
            },
            state: LoginState::Running,
            lock: None,
        });
        let output = b"secret\r\n\x1b]0;secret title\x07\x1b]9;secret notification\x07\x1b]777;notify;secret;body\x07\x1b]52;c;c2VjcmV0\x07\x1b]7;file://secret/secret\x07\x1b]133;A\x07\x1b]133;B\x07secret\x1b]133;C\x07\x1b]133;D;1\x07\x07";
        for byte in output {
            assert!(pane.feed(&[*byte]).is_empty());
        }
        assert!(pane.text(None).contains("secret"));
        assert!(pane.title.is_empty());
        assert!(pane.reported_cwd.is_none());
        assert!(!pane.bell);
        assert!(pane.scrollback_ansi().is_none());
    }
}
