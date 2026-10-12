//! The command line: every command, its arguments and their help.
#![allow(
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags,
    reason = "the doc comments are clap's help text, printed as written: `[...]` is optional, `<...>` a value"
)]

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::protocol::Placement;

#[derive(Parser)]
#[command(
    name = "kraal",
    version,
    about = "Terminal workspaces for SSH fleets and coding agents",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Session to use (one server per session). Default: inside a pane, its session; else
    /// `$KRAAL_SESSION_NAME`, else `main`.
    #[arg(short, long, global = true)]
    pub session: Option<String>,
    #[command(subcommand)]
    pub command: Option<Cmd>,
    /// Workspace to open when attaching.
    pub workspace: Option<String>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Attach to the session (starting it if needed), optionally opening a workspace. With
    /// --host, attach to the kraal running on that server, reconnecting by itself if the
    /// connection drops; the sidebar then lists that session's agents.
    Attach {
        workspace: Option<String>,
        /// A server from `kraal servers`, or user@host[:port].
        #[arg(long)]
        host: Option<String>,
        /// With --host: show this pane of the session first (an id or a name).
        #[arg(long, requires = "host")]
        pane: Option<String>,
    },
    /// Install this kraal binary on a server (same OS and architecture), for `attach --host`.
    RemoteInstall { server: String },
    /// Replace this kraal with the latest release from GitHub, after checking its checksum.
    Update {
        /// Only say whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// A release tag to install instead of the latest, e.g. v0.2.0.
        #[arg(long, value_name = "TAG")]
        to: Option<String>,
        /// Install even when it is the version already here.
        #[arg(long)]
        force: bool,
        /// Restart the sessions running an older kraal without asking.
        #[arg(long, conflicts_with = "no_restart")]
        restart: bool,
        /// Leave the sessions running an older kraal as they are, without asking.
        #[arg(long)]
        no_restart: bool,
    },
    /// Relay a client to the local session (what `attach --host` runs on the server).
    #[command(hide = true)]
    Proxy,
    /// List sessions (running and saved).
    Ls,
    /// Stop a session's server; its layout is saved for next time.
    Kill,
    /// Restart a session's server on the kraal installed now: the layout, scrollback, SSH shells
    /// and agents come back, other programs stop (they are listed first, and asked about).
    Restart {
        /// Do not ask, even when programs would stop.
        #[arg(short, long)]
        yes: bool,
    },
    /// Forget a session's saved layout and scrollback (and its paste buffers, when they are kept).
    Forget,
    /// List workspaces: open ones and definitions on disk.
    #[command(alias = "ws")]
    Workspaces {
        #[arg(long)]
        json: bool,
    },
    /// Start a workspace from its definition (or switch to it if open).
    Open { name: String },
    /// List panes.
    Panes {
        #[arg(long)]
        json: bool,
    },
    /// List coding agents and their state.
    Agents {
        #[arg(long)]
        json: bool,
        /// Print the list as one JSON line, then again each time it changes; a session that is
        /// not running has no agents until it starts.
        #[arg(long)]
        follow: bool,
    },
    /// Run a command in a new pane.
    Run(RunArgs),
    /// Show a file (markdown, image, text) or directory in a new pane.
    Preview {
        path: PathBuf,
        #[command(flatten)]
        place: PlaceArgs,
    },
    /// Type text into a pane.
    Send {
        #[arg(short, long)]
        pane: Option<String>,
        /// Press Enter after the text.
        #[arg(short, long)]
        enter: bool,
        /// The pane is production and you mean to type there.
        #[arg(long)]
        production: bool,
        text: Vec<String>,
    },
    /// Press keys in a pane (e.g. `enter`, `ctrl+c`, `up`).
    Keys {
        #[arg(short, long)]
        pane: Option<String>,
        /// The pane is production and you mean to type there.
        #[arg(long)]
        production: bool,
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Print a pane's screen, or its last N lines with --lines.
    Read {
        #[arg(short, long)]
        pane: Option<String>,
        #[arg(short, long)]
        lines: Option<usize>,
    },
    /// Wait until a pane's agent is in one of the given states (working, blocked, done, idle),
    /// or its program exits (exit) or stops printing (quiet).
    Wait {
        #[arg(short, long)]
        pane: Option<String>,
        #[arg(short, long, value_delimiter = ',')]
        until: Vec<String>,
        /// Seconds.
        #[arg(short, long)]
        timeout: Option<f64>,
    },
    /// Focus a pane in the attached client.
    Focus { pane: String },
    /// Close a pane.
    Close { pane: Option<String> },
    /// Rename a pane.
    Rename {
        #[arg(short, long)]
        pane: Option<String>,
        name: String,
    },
    /// Show a desktop notification through the attached terminal.
    Notify { title: String, body: Option<String> },
    /// Paste buffers: what kraal copied (copy mode, mouse selections, hints, the last command's
    /// output), newest first, and text kept by name. Numbered buffers (0 is the newest) rotate;
    /// named ones stay until deleted.
    Buffer {
        #[command(subcommand)]
        action: BufferCmd,
    },
    /// Runbooks: procedures with a plan, run on servers (TOML), and documents run step by step
    /// (Markdown). Yours are in ~/.config/kraal/runbooks, a repository's in .kraal/runbooks.
    Runbook {
        #[command(subcommand)]
        action: RunbookCmd,
    },
    /// Print what happens in the session as JSON lines: `kraal events 'agent.*' pane.failed`.
    Events {
        /// Event types to follow (`pane.exited`, or a prefix like `agent.*`); all by default.
        types: Vec<String>,
        /// List the event types and exit.
        #[arg(long)]
        list: bool,
        /// First replay the events numbered above this `seq` that the server still has (an
        /// `events.lost` line says when some are gone).
        #[arg(long)]
        since: Option<u64>,
        /// The `boot` the `--since` cursor comes from: after a restart, everything is replayed.
        #[arg(long, requires = "since")]
        boot: Option<u64>,
    },
    /// Save a workspace's current layout to its definition file, or as `<name>.kdl`.
    Save {
        name: Option<String>,
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Copy Zellij layouts into kraal's workspace directory.
    ImportZellij {
        #[arg(long)]
        from: Option<PathBuf>,
        /// Overwrite existing kraal workspaces with the same name.
        #[arg(long)]
        force: bool,
    },
    /// Copy files between this machine and servers, or between servers:
    /// `kraal cp report.pdf db:/tmp/`, `kraal cp db:/var/log/app.log .`, `kraal cp a:/data b:/backup`.
    Cp {
        #[arg(required = true, num_args = 2..)]
        paths: Vec<String>,
        /// Print the commands instead of running them.
        #[arg(long)]
        dry_run: bool,
        /// Delete the sources once the copy succeeded.
        #[arg(long = "move")]
        move_: bool,
        /// Refuse to replace what is already in the destination directory (say what it is).
        #[arg(long)]
        no_replace: bool,
        /// With --no-replace: a name that may be replaced all the same (repeatable).
        #[arg(long = "allow", value_name = "NAME", requires = "no_replace")]
        allow: Vec<String>,
        /// SERVER=SSH: refuse unless SERVER is still reached with these ssh arguments (the file
        /// manager passes what its panel listed).
        #[arg(long, value_name = "SERVER=SSH", hide = true)]
        expect: Vec<String>,
    },
    /// Delete local or remote (`server:path`) files and directories.
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long, value_name = "SERVER=SSH", hide = true)]
        expect: Vec<String>,
    },
    /// Open a two-panel file manager: `kraal files ~/Dev db:/var/log`.
    Files { left: Option<String>, right: Option<String> },
    /// Edit a server's file in your $VISUAL or $EDITOR, in a floating pane of kraal; each save
    /// goes back to the server: `kraal edit db:/etc/app.conf`.
    Edit {
        /// SERVER:PATH (a path relative to the home directory there, or absolute).
        target: String,
        /// Open the editor at this line.
        #[arg(long)]
        line: Option<u32>,
        /// The server is production and you mean to edit there.
        #[arg(long)]
        production: bool,
        /// Edit it as root, through sudo on the server: a pane asks for your password, which
        /// kraal hands to sudo and does not keep.
        #[arg(long)]
        sudo: bool,
    },
    /// Your servers: list, add, remove, import from workspaces, edit (~/.config/kraal/servers.toml).
    Servers {
        #[command(subcommand)]
        action: Option<ServersCmd>,
    },
    /// Connect to a server by name: plain ssh here, or a new pane with
    /// --right/--down/--tab/--floating, or every server of a group in a grid with --group.
    Ssh {
        target: String,
        /// Open the whole group TARGET in a grid.
        #[arg(short, long)]
        group: bool,
        #[arg(long, conflicts_with_all = ["down", "floating", "tab"])]
        right: bool,
        #[arg(long, conflicts_with_all = ["floating", "tab"])]
        down: bool,
        #[arg(long, conflicts_with = "tab")]
        floating: bool,
        #[arg(long)]
        tab: bool,
        /// Command to run on the server instead of a shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Agent accounts: several subscriptions per agent (Claude, Codex), with usage thresholds.
    Accounts {
        #[command(subcommand)]
        action: Option<AccountsCmd>,
    },
    /// Start an agent on the right account: in a new pane inside kraal (with
    /// --right/--down/--tab/--floating, or by default), else right here.
    Agent {
        /// claude or codex.
        agent: String,
        #[arg(short, long)]
        account: Option<String>,
        #[arg(long, conflicts_with_all = ["down", "floating", "tab", "here"])]
        right: bool,
        #[arg(long, conflicts_with_all = ["floating", "tab", "here"])]
        down: bool,
        #[arg(long, conflicts_with_all = ["tab", "here"])]
        floating: bool,
        #[arg(long, conflicts_with = "here")]
        tab: bool,
        /// Run in this terminal even inside kraal.
        #[arg(long)]
        here: bool,
        /// Arguments for the agent.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run a command on several servers at once and show each result:
    /// `kraal exec @prod -- uptime`, `kraal exec web01 db -- 'df -h | grep /dev'`.
    Exec {
        /// Server names, user@host, or @group.
        #[arg(required = true)]
        targets: Vec<String>,
        /// The remote command line (as `ssh host command` takes it).
        #[arg(last = true, required = true)]
        command: Vec<String>,
        /// How many servers at a time.
        #[arg(short, long, default_value_t = 16)]
        parallel: usize,
        /// Seconds before giving up on a server.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// SSH tunnels kraal keeps open (and reopens after a drop or a restart):
    /// `kraal tunnel add db 5432`, `kraal tunnel list`, `kraal tunnel rm 5432`.
    Tunnel {
        #[command(subcommand)]
        action: TunnelCmd,
    },
    /// Git worktrees for agents: each one gets its own branch (kraal/<name>) and directory.
    Wt {
        #[command(subcommand)]
        action: WtCmd,
    },
    /// Status line for Claude Code: records the account's usage, then runs your own status line.
    #[command(hide = true)]
    Statusline,
    /// Report agent state from inside a pane (used by agent hooks).
    #[command(hide = true)]
    Hook { agent: String, payload: Option<String> },
    /// Run a program, then end whatever it left running (kraal's own helper runs).
    #[command(hide = true)]
    Supervise {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<std::ffi::OsString>,
    },
    /// Install, remove or check the agent hooks that report exact states, or the shell lines
    /// that mark prompts and commands (show prints those lines).
    Integration {
        #[arg(value_parser = ["install", "uninstall", "status", "show"])]
        action: String,
        /// claude, codex, gemini, qwen, droid, codebuddy, auggie, cursor, copilot, kimi, opencode,
        /// kilo, pi, zsh, bash, fish, or all (the agents used here; status also checks the shells).
        #[arg(default_value = "all")]
        agent: String,
    },
    /// Configuration helpers: `default` prints every setting, `path` where the file is, `check`
    /// validates it, `keys` prints every key, `reload` makes the running server read it again.
    Config {
        #[arg(value_parser = ["default", "path", "check", "keys", "reload"])]
        action: String,
        /// With `keys`: Markdown tables, to keep or print.
        #[arg(long)]
        markdown: bool,
    },
    /// Run a session server in the foreground (normally started automatically).
    #[command(hide = true)]
    Server,
}

#[derive(Subcommand)]
pub enum BufferCmd {
    /// List the buffers, newest first: number or name, size, age, first line.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a buffer's text: by number or name, the newest by default.
    Show { buffer: Option<String> },
    /// Keep text in a buffer: named (it stays until deleted), or numbered without a name. The
    /// text comes from --from-file, else from the standard input.
    Set {
        name: Option<String>,
        #[arg(short, long, value_name = "FILE")]
        from_file: Option<PathBuf>,
    },
    /// Paste a buffer into a pane, as your terminal pastes: bracketed when the program asked for
    /// it. By number or name, the newest by default.
    Paste {
        buffer: Option<String>,
        #[arg(short, long)]
        pane: Option<String>,
        /// The pane is production and you mean to paste there.
        #[arg(long)]
        production: bool,
    },
    /// Delete a buffer (by number or name, the newest by default), or all of them.
    Delete {
        buffer: Option<String>,
        #[arg(long, conflicts_with = "buffer")]
        all: bool,
    },
}

#[derive(Subcommand)]
pub enum TunnelCmd {
    /// Forward `[host:]port` as seen from the server to a local port.
    Add {
        server: String,
        remote: String,
        /// Local port (default: the same as the remote one when free).
        #[arg(short, long)]
        local: Option<u16>,
    },
    /// List the tunnels: state, server and port, and the local port each one is on.
    List,
    /// Close a tunnel, by its local port (`kraal tunnel list` shows it).
    Rm { port: String },
}

#[derive(Subcommand)]
pub enum WtCmd {
    /// Create a worktree from the current repository and prepare it (.kraal/worktree.toml and
    /// [worktrees] in the config: files copied, setup commands); with --agent, start that agent
    /// in it (in a new tab inside kraal).
    New {
        name: String,
        #[arg(long)]
        agent: Option<String>,
        /// Branch to start from (default: the current one).
        #[arg(long)]
        base: Option<String>,
        /// Trust the repository's setup commands without asking, when they are new or changed.
        #[arg(long)]
        yes: bool,
        /// Copy nothing and run no setup command.
        #[arg(long)]
        no_setup: bool,
    },
    /// List this repository's kraal worktrees, with their number.
    List,
    /// Remove a worktree and delete its branch.
    Rm {
        name: String,
        /// Even with uncommitted changes or an unmerged branch.
        #[arg(long)]
        force: bool,
        /// Even with ignored files the main checkout does not have (build output, local
        /// databases): they are deleted with it.
        #[arg(long)]
        discard_ignored: bool,
    },
    /// Remove the worktrees merged into their base and clean (no changes, every commit or its
    /// changes in their base, no ignored file of their own), with their branches; says why the
    /// others stay.
    Prune {
        /// Only say what would be removed.
        #[arg(long)]
        dry_run: bool,
        /// Also remove merged worktrees with ignored files the main checkout does not have
        /// (build output, local databases): they are deleted with them.
        #[arg(long)]
        discard_ignored: bool,
    },
}

#[derive(Subcommand)]
pub enum AccountsCmd {
    /// List accounts with their usage (the default).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a subscription: `kraal accounts add claude pro`, then log in with the agent's own login.
    Add {
        agent: String,
        name: String,
        /// Config home (default: ~/.local/share/kraal/accounts/<agent>-<name>; the agent's default
        /// home adopts the account you already have).
        #[arg(long)]
        home: Option<String>,
        #[arg(long)]
        threshold: Option<u8>,
        #[arg(long)]
        no_login: bool,
    },
    /// Log an account in again (runs the agent's own login).
    Login { account: String },
    /// Forget an account (its directory is kept).
    Rm { account: String },
    /// Rename an account (`kraal accounts rename claude:pro work`): its rules and usage follow, its
    /// directory stays.
    Rename { account: String, name: String },
    /// Ask Codex accounts for their current usage now.
    Refresh,
    /// Set the usage threshold (percent) and what happens there: notify, ask or auto.
    Limits {
        #[arg(long)]
        threshold: Option<u8>,
        #[arg(long, value_parser = ["notify", "ask", "auto"])]
        mode: Option<String>,
    },
    /// Edit accounts.toml (accounts, rules, limits) in $VISUAL or $EDITOR.
    Edit,
}

#[derive(Subcommand)]
pub enum RunbookCmd {
    /// List your runbooks and those of the repository you are in.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a runbook: its file and what it would run.
    Show { name: String },
    /// Approve a repository's runbook as it is now, so that it can run: read it first (`kraal
    /// runbook show`). When it changes, it needs your approval again.
    Trust { name: String },
    /// Run a step runbook in kraal: shows the plan, asks, then runs it. A repository's runbook
    /// that is new or changed is shown first, for you to approve.
    Run {
        name: String,
        /// Where to run it: a server, user@host or @group.
        #[arg(long)]
        on: Option<String>,
        /// A parameter, NAME=value (repeat for several).
        #[arg(short, long = "param")]
        params: Vec<String>,
        /// Run without asking about the plan. It approves nothing: a repository's runbook that
        /// is new or changed is refused (`kraal runbook trust`).
        #[arg(short, long)]
        yes: bool,
        /// Show the plan only.
        #[arg(long)]
        dry_run: bool,
        /// Wait until it finishes; exit with an error if a target failed or was skipped.
        #[arg(long)]
        wait: bool,
    },
    /// Create a runbook from a template and print its path.
    New {
        name: String,
        /// A Markdown document whose shell blocks run one by one.
        #[arg(long)]
        markdown: bool,
        /// In the repository you are in (.kraal/runbooks), to share it with the code.
        #[arg(long)]
        repo: bool,
    },
    /// Open a runbook in $VISUAL or $EDITOR.
    Edit { name: String },
}

#[derive(Subcommand)]
pub enum ServersCmd {
    /// List servers (the default).
    List {
        #[arg(long)]
        json: bool,
        #[arg(short, long)]
        group: Option<String>,
    },
    /// Add a server, e.g. `kraal servers add admin@db.lan:2222 --name db --group prod`.
    Add {
        destination: String,
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short, long)]
        group: Vec<String>,
        /// ProxyJump: a server name or user@host[:port].
        #[arg(short = 'J', long)]
        jump: Option<String>,
        #[arg(short, long)]
        identity: Option<String>,
        /// Stage of the machine: "prod" gives its panes a red frame and the production guard.
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// The kraal session on the server whose agents the sidebar lists (e.g. main).
        #[arg(long, value_name = "SESSION")]
        agents: Option<String>,
    },
    /// Remove a server from the inventory.
    Rm { name: String },
    /// Copy the servers found in your workspace files into the inventory.
    Import,
    /// Open the inventory in $VISUAL or $EDITOR.
    Edit,
}

#[derive(Args)]
pub struct PlaceArgs {
    /// Pane to split (default: the current pane, or the focused one).
    #[arg(short, long)]
    pub target: Option<String>,
    #[arg(long, conflicts_with_all = ["down", "floating", "tab"])]
    right: bool,
    #[arg(long, conflicts_with_all = ["floating", "tab"])]
    down: bool,
    #[arg(long, conflicts_with = "tab")]
    pub floating: bool,
    #[arg(long)]
    tab: bool,
    /// Keep the focus where it is.
    #[arg(long)]
    pub no_focus: bool,
    #[arg(short, long)]
    pub name: Option<String>,
}

impl PlaceArgs {
    pub fn placement(&self) -> Placement {
        if self.right {
            Placement::Right
        } else if self.down {
            Placement::Down
        } else if self.floating {
            Placement::Floating
        } else if self.tab {
            Placement::Tab
        } else {
            Placement::Auto
        }
    }
}

#[derive(Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub place: PlaceArgs,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Close the pane when the command exits instead of showing its exit code.
    #[arg(short = 'x', long)]
    pub close_on_exit: bool,
    #[arg(trailing_var_arg = true, required = true)]
    pub command: Vec<String>,
}
