//! The agent manager in the server: launching agents on accounts, the agent picker, usage
//! thresholds, and continuing a conversation on another account.

use std::path::PathBuf;
use std::time::Instant;

use super::ClientId;
use super::panes::ProgramStop;
use crate::account_status::Status;
use crate::accounts::Account;
use crate::pane::PaneSpec;

mod choices;
mod discovery;
mod launch;
mod limits;
mod login;
mod switching;

pub(super) use login::login_start_failed;

#[derive(Default)]
pub(super) struct Discovery {
    checks: std::collections::HashMap<String, ExistingCheck>,
    generations: std::collections::HashMap<String, u64>,
    identities: std::collections::HashMap<String, (Account, crate::account_status::Checked)>,
    observed: std::collections::HashMap<crate::layout::PaneId, ObservedAgent>,
}

#[derive(Default)]
pub(super) struct AccountSwitches {
    pending: std::collections::HashMap<crate::layout::PaneId, AccountSwitch>,
}

/// What kraal last did about a pane being over its threshold, so it does not repeat itself.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitAlert {
    Notified,
    Asked,
    Attempted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwitchStage {
    Checking,
    Verifying,
    Waiting,
    Confirm,
    Stopping,
    Transferring,
    RecoveryChecking,
    Starting,
    Restoring,
    Recovering,
    RecoveryStopping,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingCheck {
    Checking,
    Checked(Status),
    SaveFailed,
}

pub(super) struct AccountSwitch {
    token: u32,
    from: Account,
    to: Account,
    session: Option<String>,
    process: Option<crate::pane::ProcessHold>,
    client: Option<ClientId>,
    ask: bool,
    original: PaneSpec,
    undo: bool,
    observed: Option<ObservedAgent>,
    started: Option<Instant>,
    stopping: Option<ProgramStop>,
    pub stage: SwitchStage,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ObservedAgent {
    pid: u32,
    started: u64,
    command: Vec<String>,
    pub(super) account: Account,
    session: String,
    cwd: PathBuf,
    path: String,
}

/// Why a pane waits for Enter before its agent starts again.
pub(super) const AMBIGUOUS: &str =
    "kraal cannot tell this command's options from a prompt it would send again: check it, Enter runs it as shown";

#[cfg(test)]
mod tests;
