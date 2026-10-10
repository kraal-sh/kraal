//! kraal — a terminal workspace manager for SSH fleets and coding agents.

mod accounts;
mod agent;
mod ansi;
mod client;
mod codex_usage;
mod config;
mod files;
mod fleet;
mod git;
mod input;
mod integration;
mod layout;
mod pane;
mod paths;
mod persist;
mod preview;
mod protocol;
mod server;
mod servers;
mod theme;
mod transfer;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::protocol::{PaneInfo, Placement, Request, Response};

#[derive(Parser)]
#[command(
    name = "kraal",
    version,
    about = "Terminal workspaces for SSH fleets and coding agents",
    disable_help_subcommand = true
)]
struct Cli {
    /// Session to use (one server per session).
    #[arg(short, long, global = true, env = "KRAAL_SESSION_NAME")]
    session: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Workspace to open when attaching.
    workspace: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach to the session (starting it if needed), optionally opening a workspace. With
    /// --host, attach to the kraal running on that server, reconnecting by itself if the
    /// connection drops.
    Attach {
        workspace: Option<String>,
        /// A server from `kraal servers`, or user@host[:port].
        #[arg(long)]
        host: Option<String>,
    },
    /// Install this kraal binary on a server (same OS and architecture), for `attach --host`.
    RemoteInstall { server: String },
    /// Relay a client to the local session (what `attach --host` runs on the server).
    #[command(hide = true)]
    Proxy,
    /// List sessions (running and saved).
    Ls,
    /// Stop a session's server; its layout is saved for next time.
    Kill,
    /// Forget a session's saved layout.
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
        text: Vec<String>,
    },
    /// Press keys in a pane (e.g. `enter`, `ctrl+c`, `up`).
    Keys {
        #[arg(short, long)]
        pane: Option<String>,
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
    /// Save a workspace's current layout as a definition file.
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
    },
    /// Delete local or remote (`server:path`) files and directories.
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Open a two-panel file manager: `kraal files ~/Dev db:/var/log`.
    Files { left: Option<String>, right: Option<String> },
    /// Your servers: list, add, remove, import from workspaces, edit (~/.config/kraal/servers.toml).
    Servers {
        #[command(subcommand)]
        action: Option<ServersCmd>,
    },
    /// Connect to a server by name: plain ssh here, or a new pane with --right/--down/--tab/
    /// --floating, or every server of a group in a grid with --group.
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
    /// Start an agent on the right account: in a new pane inside kraal (with --right/--down/--tab/
    /// --floating, or by default), else right here.
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
    /// Install, remove or check the agent hooks that report exact states.
    Integration {
        #[arg(value_parser = ["install", "uninstall", "status"])]
        action: String,
        /// claude, codex or all.
        #[arg(default_value = "all")]
        agent: String,
    },
    /// Configuration helpers.
    Config {
        #[arg(value_parser = ["default", "path", "check"])]
        action: String,
    },
    /// Run a session server in the foreground (normally started automatically).
    #[command(hide = true)]
    Server,
}

#[derive(Subcommand)]
enum TunnelCmd {
    /// Forward `[host:]port` as seen from the server to a local port.
    Add {
        server: String,
        remote: String,
        /// Local port (default: the same as the remote one when free).
        #[arg(short, long)]
        local: Option<u16>,
    },
    List,
    /// Close a tunnel (by local port).
    Rm {
        port: String,
    },
}

#[derive(Subcommand)]
enum WtCmd {
    /// Create a worktree from the current repository; with --agent, start that agent in it
    /// (in a new tab inside kraal).
    New {
        name: String,
        #[arg(long)]
        agent: Option<String>,
        /// Branch to start from (default: the current one).
        #[arg(long)]
        base: Option<String>,
    },
    /// List this repository's kraal worktrees.
    List,
    /// Remove a worktree and delete its branch.
    Rm {
        name: String,
        /// Even with uncommitted changes or an unmerged branch.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum AccountsCmd {
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
    /// Ask Codex accounts for their current usage now.
    Refresh,
    /// Set the usage threshold (percent) and what happens there: notify, ask or auto.
    Limits {
        #[arg(long)]
        threshold: Option<u8>,
        #[arg(long, value_parser = ["notify", "ask", "auto"])]
        mode: Option<String>,
    },
    /// Edit accounts.toml (accounts, rules, limits) in $EDITOR.
    Edit,
}

#[derive(Subcommand)]
enum ServersCmd {
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
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove a server from the inventory.
    Rm { name: String },
    /// Copy the servers found in your workspace files into the inventory.
    Import,
    /// Open the inventory in $EDITOR.
    Edit,
}

#[derive(Args)]
struct PlaceArgs {
    /// Pane to split (default: the current pane, or the focused one).
    #[arg(short, long)]
    target: Option<String>,
    #[arg(long, conflicts_with_all = ["down", "floating", "tab"])]
    right: bool,
    #[arg(long, conflicts_with_all = ["floating", "tab"])]
    down: bool,
    #[arg(long, conflicts_with = "tab")]
    floating: bool,
    #[arg(long)]
    tab: bool,
    /// Keep the focus where it is.
    #[arg(long)]
    no_focus: bool,
    #[arg(short, long)]
    name: Option<String>,
}

impl PlaceArgs {
    fn placement(&self) -> Placement {
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

    fn target(&self) -> Option<String> {
        self.target.clone().or_else(|| std::env::var("KRAAL_PANE").ok())
    }
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    place: PlaceArgs,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Close the pane when the command exits instead of showing its exit code.
    #[arg(short = 'x', long)]
    close_on_exit: bool,
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    // Plain CLI commands should die quietly when piped into `head`; the server and the attached
    // client handle broken sockets themselves.
    if !matches!(cli.command, None | Some(Cmd::Attach { .. }) | Some(Cmd::Server)) {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    if let Err(e) = run(cli) {
        eprintln!("kraal: {e:#}");
        std::process::exit(1);
    }
}

fn session_name(cli: &Option<String>) -> Result<String> {
    let s = cli.clone().unwrap_or_else(|| "main".into());
    if !paths::valid_session_name(&s) {
        bail!("invalid session name {s:?}");
    }
    Ok(s)
}

/// The pane a command acts on: explicit, else the pane the command runs in.
fn pane_ref(p: Option<String>) -> Result<String> {
    p.or_else(|| std::env::var("KRAAL_PANE").ok())
        .context("which pane? pass --pane (an id or a name; see `kraal panes`)")
}

fn run(cli: Cli) -> Result<()> {
    let explicit_session = cli.session.clone();
    let session = session_name(&cli.session)?;
    let socket = || client::control_socket(explicit_session.as_deref());
    let req = |r: Request| client::request(&socket(), r);
    match cli.command {
        None => attach(&session, cli.workspace),
        Some(Cmd::Attach { workspace, host: None }) => attach(&session, workspace),
        Some(Cmd::Attach { workspace, host: Some(host) }) => attach_remote(&session, &host, workspace),
        Some(Cmd::RemoteInstall { server }) => remote_install(&server),
        Some(Cmd::Proxy) => client::proxy(&session),
        Some(Cmd::Server) => server::run(&session),
        Some(Cmd::Ls) => list_sessions(),
        Some(Cmd::Kill) => {
            client::request(&paths::socket_path(&session), Request::KillServer)?;
            println!("stopped session {session} (layout saved)");
            Ok(())
        }
        Some(Cmd::Forget) => {
            if std::os::unix::net::UnixStream::connect(paths::socket_path(&session)).is_ok() {
                bail!("session {session} is running; `kraal kill` it first");
            }
            let p = paths::session_state_path(&session);
            match std::fs::remove_file(&p) {
                Ok(()) => println!("forgot {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("nothing saved for {session}"),
                Err(e) => return Err(e.into()),
            }
            Ok(())
        }
        Some(Cmd::Workspaces { json }) => {
            let r = match req(Request::ListWorkspaces) {
                Ok(r) => r,
                Err(_) => return list_definitions(json),
            };
            let Response::Workspaces(list) = r else { bail!("unexpected answer") };
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            for w in list {
                let state = if w.open { "open" } else { "-" };
                let agents = if w.blocked + w.done + w.working > 0 {
                    format!("  ▲{} ✓{} ●{}", w.blocked, w.done, w.working)
                } else {
                    String::new()
                };
                println!("{:<20} {:<5} {:>3} tabs {:>4} panes{agents}", w.name, state, w.tabs, w.panes);
            }
            Ok(())
        }
        Some(Cmd::Open { name }) => {
            client::ensure_server(&session)?;
            req(Request::OpenWorkspace { name: name.clone() })?;
            println!("workspace {name} is open; `kraal attach {name}` to see it");
            Ok(())
        }
        Some(Cmd::Panes { json }) => {
            let Response::Panes(list) = req(Request::ListPanes)? else { bail!("unexpected answer") };
            print_panes(&list, json, false)
        }
        Some(Cmd::Agents { json }) => {
            let Response::Panes(list) = req(Request::ListPanes)? else { bail!("unexpected answer") };
            let list: Vec<PaneInfo> = list.into_iter().filter(|p| p.agent.is_some()).collect();
            print_panes(&list, json, true)
        }
        Some(Cmd::Run(args)) => {
            let cwd = args.cwd.map(std::path::absolute).transpose()?.or_else(|| std::env::current_dir().ok());
            let r = req(Request::NewPane {
                target: args.place.target(),
                command: args.command,
                cwd,
                name: args.place.name.clone(),
                placement: args.place.placement(),
                preview: None,
                focus: !args.place.no_focus,
                close_on_exit: args.close_on_exit,
            })?;
            if let Response::Pane(p) = r {
                println!("{}", p.id);
            }
            Ok(())
        }
        Some(Cmd::Preview { path, place }) => {
            let path = std::path::absolute(&path)?;
            if !path.exists() {
                bail!("{} does not exist", path.display());
            }
            let r = req(Request::NewPane {
                target: place.target(),
                command: Vec::new(),
                cwd: path.parent().map(PathBuf::from),
                name: place.name.clone(),
                placement: place.placement(),
                preview: Some(path),
                focus: !place.no_focus,
                close_on_exit: false,
            })?;
            if let Response::Pane(p) = r {
                println!("{}", p.id);
            }
            Ok(())
        }
        Some(Cmd::Send { pane, enter, text }) => {
            let mut t = text.join(" ");
            if enter {
                t.push('\r');
            }
            req(Request::SendText { pane: pane_ref(pane)?, text: t })?;
            Ok(())
        }
        Some(Cmd::Keys { pane, keys }) => {
            req(Request::SendKeys { pane: pane_ref(pane)?, keys })?;
            Ok(())
        }
        Some(Cmd::Read { pane, lines }) => {
            if let Response::Text(t) = req(Request::ReadPane { pane: pane_ref(pane)?, lines })? {
                print!("{t}");
            }
            Ok(())
        }
        Some(Cmd::Wait { pane, until, timeout }) => {
            let r =
                req(Request::Wait { pane: pane_ref(pane)?, until, timeout_ms: timeout.map(|t| (t * 1000.0) as u64) })?;
            if let Response::Pane(p) = r {
                println!("{}", p.state.label());
            }
            Ok(())
        }
        Some(Cmd::Focus { pane }) => req(Request::FocusPane { pane }).map(|_| ()),
        Some(Cmd::Close { pane }) => req(Request::ClosePane { pane: pane_ref(pane)? }).map(|_| ()),
        Some(Cmd::Rename { pane, name }) => req(Request::RenamePane { pane: pane_ref(pane)?, name }).map(|_| ()),
        Some(Cmd::Notify { title, body }) => req(Request::Notify { title, body: body.unwrap_or_default() }).map(|_| ()),
        Some(Cmd::Save { name, workspace }) => {
            if let Response::Path(p) = req(Request::SaveWorkspace { workspace, name })? {
                println!("saved {}", p.display());
            }
            Ok(())
        }
        Some(Cmd::ImportZellij { from, force }) => import_zellij(from, force),
        Some(Cmd::Cp { paths, dry_run, move_ }) => copy(&paths, dry_run, move_),
        Some(Cmd::Rm { paths }) => remove(&paths),
        Some(Cmd::Files { left, right }) => {
            if std::env::var_os("KRAAL_SOCKET").is_none() {
                client::ensure_server(&session)?;
            }
            let home = || dirs::home_dir().unwrap_or_default().display().to_string();
            let abs = |s: String| -> String {
                if s.contains(':') && !s.starts_with('/') && !s.starts_with('.') {
                    return s;
                }
                std::path::absolute(paths::expand_tilde(&s)).map(|p| p.display().to_string()).unwrap_or(s)
            };
            let left = left
                .map(abs)
                .unwrap_or_else(|| std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| home()));
            let right = right.map(abs).unwrap_or_else(home);
            req(Request::OpenFiles { left, right, near: std::env::var("KRAAL_PANE").ok() })?;
            Ok(())
        }
        Some(Cmd::Servers { action }) => servers_cmd(action.unwrap_or(ServersCmd::List { json: false, group: None })),
        Some(Cmd::Ssh { target, group, right, down, floating, tab, command }) => {
            let placement = if right {
                Some(Placement::Right)
            } else if down {
                Some(Placement::Down)
            } else if floating {
                Some(Placement::Floating)
            } else if tab {
                Some(Placement::Tab)
            } else {
                None
            };
            if group || placement.is_some() {
                if std::env::var_os("KRAAL_SOCKET").is_none() {
                    client::ensure_server(&session)?;
                }
                req(Request::OpenSsh {
                    target,
                    group,
                    placement: placement.unwrap_or_default(),
                    near: std::env::var("KRAAL_PANE").ok(),
                    command: command.clone(),
                })?;
                return Ok(());
            }
            exec_ssh(&target, &command)
        }
        Some(Cmd::Accounts { action }) => accounts_cmd(action.unwrap_or(AccountsCmd::List { json: false })),
        Some(Cmd::Agent { agent, account, right, down, floating, tab, here, args }) => {
            let placement = if right {
                Placement::Right
            } else if down {
                Placement::Down
            } else if floating {
                Placement::Floating
            } else if tab {
                Placement::Tab
            } else {
                Placement::Auto
            };
            let inside = std::env::var_os("KRAAL_SOCKET").is_some();
            if inside && !here && args.is_empty() {
                req(Request::OpenAgent {
                    agent,
                    account,
                    placement,
                    near: std::env::var("KRAAL_PANE").ok(),
                    cwd: std::env::current_dir().ok(),
                })?;
                return Ok(());
            }
            exec_agent(&agent, account.as_deref(), &args)
        }
        Some(Cmd::Exec { targets, command, parallel, timeout, json }) => {
            exec_fleet(&targets, &command, parallel, timeout, json)
        }
        Some(Cmd::Tunnel { action }) => {
            if std::env::var_os("KRAAL_SOCKET").is_none() {
                client::ensure_server(&session)?;
            }
            let print = |list: Vec<server::net::TunnelInfo>| {
                for t in list {
                    let err = t.error.map(|e| format!("  ({e})")).unwrap_or_default();
                    println!("{:<12} {}{err}", format!("{:?}", t.state).to_lowercase(), t.spec.describe());
                }
            };
            match action {
                TunnelCmd::Add { server, remote, local } => {
                    if let Response::Tunnels(t) = req(Request::TunnelAdd { server, remote, local })? {
                        print(t);
                    }
                }
                TunnelCmd::List => {
                    if let Response::Tunnels(t) = req(Request::TunnelList)? {
                        if t.is_empty() {
                            println!("no tunnels");
                        }
                        print(t);
                    }
                }
                TunnelCmd::Rm { port } => {
                    req(Request::TunnelRemove { key: port })?;
                }
            }
            Ok(())
        }
        Some(Cmd::Wt { action }) => {
            let cwd = std::env::current_dir()?;
            match action {
                WtCmd::New { name, agent, base } => {
                    let w = git::add_worktree(&cwd, &name, base.as_deref())?;
                    println!("{} on {} (from {})", w.path.display(), w.branch, w.base);
                    if let Some(agent) = agent {
                        if std::env::var_os("KRAAL_SOCKET").is_some() {
                            req(Request::OpenAgent {
                                agent,
                                account: None,
                                placement: Placement::Tab,
                                near: std::env::var("KRAAL_PANE").ok(),
                                cwd: Some(w.path.clone()),
                            })?;
                        } else {
                            std::env::set_current_dir(&w.path)?;
                            return exec_agent(&agent, None, &[]);
                        }
                    }
                    Ok(())
                }
                WtCmd::List => {
                    for w in git::list_worktrees(&cwd)? {
                        let state = git::status(&w.path).map(|g| g.label(false)).unwrap_or_default();
                        println!("{:<24} {:<32} {}", w.name(), state, paths::shorten_home(&w.path));
                    }
                    Ok(())
                }
                WtCmd::Rm { name, force } => {
                    let w = git::list_worktrees(&cwd)?
                        .into_iter()
                        .find(|w| w.name() == name)
                        .with_context(|| format!("no kraal worktree named {name:?} here"))?;
                    git::remove_worktree(&w, force)?;
                    println!("removed {} and {}", w.path.display(), w.branch);
                    Ok(())
                }
            }
        }
        Some(Cmd::Statusline) => {
            integration::statusline();
            Ok(())
        }
        Some(Cmd::Hook { agent, payload }) => {
            integration::run_hook(&agent, payload);
            Ok(())
        }
        Some(Cmd::Integration { action, agent }) => {
            let agents: Vec<&str> = if agent == "all" { vec!["claude", "codex"] } else { vec![agent.as_str()] };
            for a in agents {
                let r = match action.as_str() {
                    "install" => integration::install(a),
                    "uninstall" => integration::uninstall(a),
                    _ => integration::status(a),
                };
                match r {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => eprintln!("{a}: {e:#}"),
                }
            }
            Ok(())
        }
        Some(Cmd::Config { action }) => match action.as_str() {
            "default" => {
                print!("{}", config::DEFAULT_CONFIG);
                Ok(())
            }
            "path" => {
                println!("{}", paths::config_path().display());
                Ok(())
            }
            _ => {
                let cfg = config::Config::load()?;
                let (_, mut warnings) = config::Keymap::build(&cfg.keys);
                warnings.extend(theme::Theme::from_config(&cfg.theme).1);
                for w in &warnings {
                    eprintln!("warning: {w}");
                }
                for (path, def) in cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)) {
                    match def {
                        Ok(d) => {
                            for w in d.warnings {
                                eprintln!("{}: {w}", path.display());
                            }
                        }
                        Err(e) => eprintln!("{}: {e:#}", path.display()),
                    }
                }
                println!("config ok");
                Ok(())
            }
        },
    }
}

fn attach(session: &str, workspace: Option<String>) -> Result<()> {
    let cfg = config::Config::load().unwrap_or_default();
    client::attach(client::Target::Local { session: session.to_string() }, workspace, cfg.mouse)
}

/// ssh arguments (up to the destination) for kraal's own connection to a server: no shared
/// connection (a stale master would delay reconnecting), and dead links detected in ~15 s.
fn remote_ssh_args(known: &[servers::Known], s: &servers::Server) -> Vec<String> {
    let mut args: Vec<String> = [
        "-T",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    args.extend(s.ssh_options(&|j| servers::resolve(known, j).map(|js| js.address())));
    args.push(s.destination());
    args
}

fn attach_remote(session: &str, host: &str, workspace: Option<String>) -> Result<()> {
    let cfg = config::Config::load().unwrap_or_default();
    let known = known_servers(&cfg)?;
    let s = servers::resolve(&known, host).with_context(|| format!("not a server: {host}"))?;
    let target = client::Target::Remote {
        ssh: remote_ssh_args(&known, &s),
        session: session.to_string(),
        label: s.name.clone(),
    };
    client::attach(target, workspace, cfg.mouse)
}

fn remote_install(server: &str) -> Result<()> {
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let s = servers::resolve(&known, server).with_context(|| format!("not a server: {server}"))?;
    let mut ssh = remote_ssh_args(&known, &s);
    let dest = ssh.pop().unwrap();
    let run = |cmd: &str| -> Result<String> {
        let out = std::process::Command::new("ssh").args(&ssh).arg(&dest).arg(cmd).output()?;
        if !out.status.success() {
            bail!("{} failed on {}: {}", cmd, s.name, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let remote = run("uname -sm")?;
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    };
    let arch = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, a) => a,
    };
    let local = format!("{os} {arch}");
    if remote != local {
        bail!("{} runs {remote}, this kraal is built for {local}; build kraal there (cargo install --path .)", s.name);
    }
    run("mkdir -p ~/.local/bin")?;
    // scp spells the port -P and has no -T.
    let mut scp: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ssh.len() {
        match ssh[i].as_str() {
            "-T" => {}
            "-p" if i + 1 < ssh.len() => {
                scp.push("-P".into());
                scp.push(ssh[i + 1].clone());
                i += 1;
            }
            a => scp.push(a.to_string()),
        }
        i += 1;
    }
    let exe = std::env::current_exe()?;
    let status =
        std::process::Command::new("scp").args(&scp).arg(&exe).arg(format!("{dest}:.local/bin/kraal.new")).status()?;
    if !status.success() {
        bail!("copying to {} failed", s.name);
    }
    run("chmod +x ~/.local/bin/kraal.new && mv ~/.local/bin/kraal.new ~/.local/bin/kraal")?;
    let version = run("~/.local/bin/kraal --version")?;
    println!("{version} installed on {} (~/.local/bin/kraal). Attach with `kraal attach --host {}`", s.name, s.name);
    Ok(())
}

fn print_panes(list: &[PaneInfo], json: bool, agents: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    for p in list {
        let name = p.name.clone().unwrap_or_else(|| {
            if p.title.is_empty() {
                p.command.first().cloned().unwrap_or_else(|| "shell".into())
            } else {
                p.title.clone()
            }
        });
        let mark = if p.focused { "*" } else { " " };
        let extra = match (&p.agent, p.exit_code) {
            _ if p.suspended => "not started (enter runs it)".to_string(),
            (_, Some(code)) => format!("exited {code}"),
            (Some(a), _) => format!("{} {a} {}", p.state.icon(), p.state.label()),
            (None, _) => p.cwd.as_ref().map(|c| paths::shorten_home(c)).unwrap_or_default(),
        };
        if agents {
            println!("{:>4}{mark} {:<14} {:<24} {extra}", p.id, p.workspace, name);
        } else {
            println!("{:>4}{mark} {:<14} {:<12} {:<24} {extra}", p.id, p.workspace, p.tab, name);
        }
    }
    Ok(())
}

fn list_sessions() -> Result<()> {
    let mut names = std::collections::BTreeMap::<String, &str>::new();
    if let Ok(rd) = std::fs::read_dir(paths::runtime_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sock")
                && let Some(stem) = p.file_stem()
            {
                let alive = std::os::unix::net::UnixStream::connect(&p).is_ok();
                if alive {
                    names.insert(stem.to_string_lossy().into(), "running");
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(paths::state_dir().join("sessions")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(stem) = p.file_stem()
            {
                names.entry(stem.to_string_lossy().into()).or_insert("saved");
            }
        }
    }
    if names.is_empty() {
        println!("no sessions");
    }
    for (n, s) in names {
        println!("{n:<20} {s}");
    }
    Ok(())
}

fn list_definitions(json: bool) -> Result<()> {
    let cfg = config::Config::load()?;
    let defs: Vec<_> = cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).collect();
    if json {
        let v: Vec<_> = defs
            .iter()
            .filter_map(|(p, d)| d.as_ref().ok().map(|d| serde_json::json!({"name": d.name, "open": false, "definition": p, "tabs": d.tabs.len(), "panes": d.panes().len()})))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for (p, d) in defs {
        match d {
            Ok(d) => println!("{:<20} {:<5} {:>3} tabs {:>4} panes", d.name, "-", d.tabs.len(), d.panes().len()),
            Err(e) => println!("{:<20} error: {e:#}", p.display()),
        }
    }
    Ok(())
}

fn import_zellij(from: Option<PathBuf>, force: bool) -> Result<()> {
    let from = from.unwrap_or_else(|| paths::expand_tilde("~/.config/zellij/layouts"));
    let to = paths::workspaces_dir();
    std::fs::create_dir_all(&to)?;
    let mut n = 0;
    for (path, def) in workspace::load_dir(&from) {
        let name = path.file_name().unwrap_or_default();
        match def {
            Err(e) => eprintln!("skipped {}: {e:#}", path.display()),
            Ok(def) => {
                let dest = to.join(name);
                if dest.exists() && !force {
                    eprintln!("skipped {} ({} exists; --force to overwrite)", path.display(), dest.display());
                    continue;
                }
                for w in &def.warnings {
                    eprintln!("{}: {w}", path.display());
                }
                std::fs::copy(&path, &dest)?;
                println!(
                    "{} → {} ({} tabs, {} panes)",
                    path.display(),
                    paths::shorten_home(&dest),
                    def.tabs.len(),
                    def.panes().len()
                );
                n += 1;
            }
        }
    }
    println!("imported {n} workspace{}", if n == 1 { "" } else { "s" });
    Ok(())
}

/// Every known server, computed without a running server.
fn known_servers(cfg: &config::Config) -> Result<Vec<servers::Known>> {
    let inv = servers::Inventory::load()?;
    let defs: Vec<workspace::WorkspaceDef> =
        cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).filter_map(|(_, d)| d.ok()).collect();
    let hosts = if cfg.ssh_config_hosts { server::ssh_config_hosts() } else { Vec::new() };
    Ok(servers::known(&inv, &defs, &hosts))
}

fn servers_cmd(action: ServersCmd) -> Result<()> {
    let cfg = config::Config::load()?;
    match action {
        ServersCmd::List { json, group } => {
            let known: Vec<servers::Known> = known_servers(&cfg)?
                .into_iter()
                .filter(|k| group.as_ref().is_none_or(|g| k.server.groups.contains(g)))
                .collect();
            if json {
                let list: Vec<&servers::Server> = known.iter().map(|k| &k.server).collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            for k in &known {
                let source = match &k.source {
                    servers::Source::Inventory => "saved".to_string(),
                    servers::Source::Workspace(w) => format!("workspace {w}"),
                    servers::Source::SshConfig => "ssh config".to_string(),
                };
                println!("{:<22} {:<42} {:<20} {source}", k.server.name, k.server.address(), k.server.groups.join(","));
            }
            if known.is_empty() {
                println!("no servers yet: `kraal servers add user@host`, or `kraal servers import`");
            }
            Ok(())
        }
        ServersCmd::Add { destination, name, group, jump, identity, note } => {
            let (user, host, port) = servers::parse_destination(&destination)
                .with_context(|| format!("not a destination: {destination}"))?;
            let mut inv = servers::Inventory::load()?;
            let s = servers::Server {
                name: name.unwrap_or_default(),
                host,
                user,
                port,
                jump,
                identity,
                groups: group,
                note,
            };
            let name = inv.add(s)?;
            inv.save()?;
            println!("saved {name} in {}", paths::shorten_home(&servers::Inventory::path()));
            Ok(())
        }
        ServersCmd::Rm { name } => {
            let mut inv = servers::Inventory::load()?;
            if !inv.remove(&name) {
                bail!("no saved server named {name:?}");
            }
            inv.save()?;
            println!("removed {name}");
            Ok(())
        }
        ServersCmd::Import => {
            let defs: Vec<workspace::WorkspaceDef> =
                cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).filter_map(|(_, d)| d.ok()).collect();
            let mut inv = servers::Inventory::load()?;
            let before = inv.servers.len();
            for k in servers::from_workspaces(&defs) {
                inv.add(k.server)?;
            }
            inv.save()?;
            println!(
                "imported {} servers into {}",
                inv.servers.len() - before,
                paths::shorten_home(&servers::Inventory::path())
            );
            Ok(())
        }
        ServersCmd::Edit => {
            let path = servers::Inventory::path();
            if !path.exists() {
                servers::Inventory::empty().save()?;
            }
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let mut parts = editor.split_whitespace();
            let status = std::process::Command::new(parts.next().unwrap_or("vi")).args(parts).arg(&path).status()?;
            if !status.success() {
                bail!("{editor} exited with {status}");
            }
            servers::Inventory::load().map(|inv| println!("{} servers", inv.servers.len()))
        }
    }
}

/// Replace this process with `ssh` to a known server.
fn exec_ssh(target: &str, command: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let s = servers::resolve(&known, target).with_context(|| format!("not a server: {target}"))?;
    let mut args = Vec::new();
    if cfg.servers.multiplex
        && let Some(path) = paths::ssh_control_path()
    {
        args.extend([
            "-o".to_string(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", path.display()),
            "-o".into(),
            "ControlPersist=10m".into(),
        ]);
    }
    args.extend(s.ssh_options(&|j| servers::resolve(&known, j).map(|js| js.address())));
    args.push(s.destination());
    args.extend(command.iter().cloned());
    let err = std::process::Command::new("ssh").args(&args).exec();
    Err(err).context("running ssh")
}

fn copy(args: &[String], dry_run: bool, move_: bool) -> Result<()> {
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let (dest, sources) = args.split_last().context("kraal cp SOURCE... DEST")?;
    let sources: Vec<transfer::Endpoint> =
        sources.iter().map(|s| transfer::parse_endpoint(s, &known)).collect::<Result<_>>()?;
    let dest = transfer::parse_endpoint(dest, &known)?;
    let control = if cfg.servers.multiplex { paths::ssh_control_path() } else { None };
    let ssh = |s: &servers::Server| -> Vec<String> {
        let mut o = Vec::new();
        if let Some(c) = &control {
            o.extend([
                "-o".to_string(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", c.display()),
                "-o".into(),
                "ControlPersist=10m".into(),
            ]);
        }
        o.extend(s.ssh_options(&|j| servers::resolve(&known, j).map(|js| js.address())));
        o
    };
    let steps = transfer::plan(&sources, &dest, &ssh, transfer::have_program("rsync"))?;
    if dry_run {
        for s in &steps {
            println!("{}", s.shell_line());
        }
        return Ok(());
    }
    let what: Vec<String> = sources.iter().map(|s| s.display()).collect();
    eprintln!("copying {} → {}", what.join(", "), dest.display());
    transfer::run(&steps)?;
    if move_ {
        transfer::remove(&sources, &ssh)?;
    }
    eprintln!("done");
    Ok(())
}

fn remove(args: &[String]) -> Result<()> {
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let targets: Vec<transfer::Endpoint> =
        args.iter().map(|s| transfer::parse_endpoint(s, &known)).collect::<Result<_>>()?;
    let control = if cfg.servers.multiplex { paths::ssh_control_path() } else { None };
    let ssh = |s: &servers::Server| -> Vec<String> {
        let mut o = Vec::new();
        if let Some(c) = &control {
            o.extend([
                "-o".to_string(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", c.display()),
            ]);
        }
        o.extend(s.ssh_options(&|j| servers::resolve(&known, j).map(|js| js.address())));
        o
    };
    transfer::remove(&targets, &ssh)?;
    eprintln!("deleted {} item{}", targets.len(), if targets.len() == 1 { "" } else { "s" });
    Ok(())
}

/// `agent:name` or a bare account name (searched across agents).
fn find_account(all: &accounts::Accounts, spec: &str) -> Result<accounts::Account> {
    if let Some((agent, name)) = spec.split_once(':').or_else(|| spec.split_once('·')) {
        return all.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"));
    }
    let matches: Vec<accounts::Account> = all.all().into_iter().filter(|a| a.name == spec).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no account named {spec:?} (see `kraal accounts`)"),
        _ => bail!("several agents have an account named {spec:?}; use agent:name"),
    }
}

/// Run a command with an account's environment in this terminal.
fn run_with(l: &accounts::Launch, exec: bool) -> Result<std::process::ExitStatus> {
    let mut cmd = std::process::Command::new(&l.argv[0]);
    cmd.args(&l.argv[1..]);
    for k in &l.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &l.env {
        cmd.env(k, v);
    }
    if exec {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        return Err(err).with_context(|| format!("running {}", l.argv[0]));
    }
    cmd.status().with_context(|| format!("running {}", l.argv[0]))
}

/// Logged in? Asks the agent itself (`claude auth status`, `codex login status`).
fn logged_in(a: &accounts::Account) -> Option<bool> {
    let mut l = accounts::login(a).ok()?;
    l.argv.pop();
    l.argv.push("status".into());
    if a.agent == "claude" {
        l.argv = vec!["claude".into(), "auth".into(), "status".into()];
    }
    let mut cmd = std::process::Command::new(&l.argv[0]);
    cmd.args(&l.argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for k in &l.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &l.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status.success());
        }
        if start.elapsed() > std::time::Duration::from_secs(10) {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

fn accounts_cmd(action: AccountsCmd) -> Result<()> {
    let mut all = accounts::Accounts::load()?;
    match action {
        AccountsCmd::List { json } => {
            let list = all.all();
            let now = accounts::now();
            let usage = accounts::UsageMap::load(&list);
            // Ask every agent in parallel whether its account is logged in.
            let status: Vec<Option<bool>> = std::thread::scope(|s| {
                let handles: Vec<_> = list.iter().map(|a| s.spawn(move || logged_in(a))).collect();
                handles.into_iter().map(|h| h.join().ok().flatten()).collect()
            });
            if json {
                let v: Vec<serde_json::Value> = list
                    .iter()
                    .zip(&status)
                    .map(|(a, st)| {
                        serde_json::json!({
                            "agent": a.agent, "name": a.name, "home": a.home, "logged_in": st,
                            "threshold": all.threshold(a), "usage": usage.get(a),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for (a, st) in list.iter().zip(&status) {
                let login = match st {
                    Some(true) => "logged in",
                    Some(false) => "not logged in",
                    None => "agent not found",
                };
                let u = usage.get(a).map(|u| server::usage_text(u, now)).unwrap_or_else(|| "no usage seen yet".into());
                println!(
                    "{:<18} {:<14} {:<34} {u}",
                    accounts::label(a),
                    login,
                    paths::shorten_home(&paths::expand_tilde(&a.home))
                );
            }
            println!("threshold {}% · when reached: {:?}", all.limits.threshold, all.limits.mode);
            Ok(())
        }
        AccountsCmd::Add { agent, name, home, threshold, no_login } => {
            let home = home.unwrap_or_else(|| accounts::default_home_for(&agent, &name).display().to_string());
            let a = accounts::Account { name, agent, home, threshold, note: None };
            all.add(a.clone())?;
            for note in accounts::prepare_home(&a)? {
                println!("  {note}");
            }
            all.save()?;
            println!("added {} ({})", accounts::label(&a), paths::shorten_home(&paths::expand_tilde(&a.home)));
            if no_login {
                return Ok(());
            }
            println!("logging in with {}'s own login…", a.agent);
            run_with(&accounts::login(&a)?, false).map(|_| ())
        }
        AccountsCmd::Login { account } => {
            let a = find_account(&all, &account)?;
            run_with(&accounts::login(&a)?, false).map(|_| ())
        }
        AccountsCmd::Rm { account } => {
            let a = find_account(&all, &account)?;
            if !all.remove(&a.agent, &a.name) {
                bail!("{} is the agent's default account; it cannot be forgotten", accounts::label(&a));
            }
            all.save()?;
            println!("forgot {} (its directory {} is kept)", accounts::label(&a), a.home);
            Ok(())
        }
        AccountsCmd::Refresh => {
            for a in all.for_agent("codex") {
                let home = paths::expand_tilde(&a.home);
                match codex_usage::fetch("codex", &home, std::time::Duration::from_secs(20)) {
                    Ok(l) => {
                        let conv = |w: Option<codex_usage::Window>| {
                            w.map(|w| accounts::Window { used_percent: w.used_percent, resets_at: w.resets_at })
                        };
                        let u = accounts::Usage {
                            five_hour: conv(l.five_hour),
                            weekly: conv(l.weekly),
                            observed_at: l.observed_at,
                            source: "codex".into(),
                        };
                        accounts::save_usage(&a.agent, &a.name, &u)?;
                        println!("{:<18} {}", accounts::label(&a), server::usage_text(&u, accounts::now()));
                    }
                    Err(e) => println!("{:<18} {e:#}", accounts::label(&a)),
                }
            }
            println!("Claude accounts report their usage through the status line while they run.");
            Ok(())
        }
        AccountsCmd::Limits { threshold, mode } => {
            if let Some(t) = threshold {
                if !(1..=100).contains(&t) {
                    bail!("the threshold is a percentage (1-100)");
                }
                all.limits.threshold = t;
            }
            if let Some(m) = mode {
                all.limits.mode = match m.as_str() {
                    "ask" => accounts::Mode::Ask,
                    "auto" => accounts::Mode::Auto,
                    _ => accounts::Mode::Notify,
                };
            }
            all.save()?;
            println!("threshold {}% · when reached: {:?}", all.limits.threshold, all.limits.mode);
            Ok(())
        }
        AccountsCmd::Edit => {
            if !accounts::Accounts::path().exists() {
                all.save()?;
            }
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let mut parts = editor.split_whitespace();
            std::process::Command::new(parts.next().unwrap_or("vi"))
                .args(parts)
                .arg(accounts::Accounts::path())
                .status()?;
            accounts::Accounts::load().map(|_| ())
        }
    }
}

/// Start an agent on the chosen account in this terminal.
fn exec_agent(agent: &str, account: Option<&str>, args: &[String]) -> Result<()> {
    let all = accounts::Accounts::load()?;
    accounts::agent_kind(agent)
        .with_context(|| format!("unknown agent {agent:?} (known: {})", accounts::agent_names()))?;
    let cwd = std::env::current_dir().ok();
    let usage = accounts::UsageMap::load(&all.all());
    let a = match account {
        Some(name) => all.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"))?,
        None => all.choose(agent, None, cwd.as_deref(), &usage, accounts::now()).context("no account")?,
    };
    let session = accounts::agent_kind(agent).and_then(|k| k.session_flag).map(|_| accounts::new_uuid());
    run_with(&accounts::launch(&a, args, None, session.as_deref())?, true).map(|_| ())
}

/// Servers named by `targets`: names, user@host, or @group.
fn resolve_targets(known: &[servers::Known], targets: &[String]) -> Result<Vec<servers::Server>> {
    let mut out: Vec<servers::Server> = Vec::new();
    for t in targets {
        let found: Vec<servers::Server> = match t.strip_prefix('@') {
            Some(group) => {
                let members: Vec<servers::Server> = known
                    .iter()
                    .filter(|k| k.server.groups.iter().any(|g| g == group))
                    .map(|k| k.server.clone())
                    .collect();
                if members.is_empty() {
                    bail!("no server in group {group:?} (see `kraal servers`)");
                }
                members
            }
            None => vec![servers::resolve(known, t).with_context(|| format!("not a server: {t}"))?],
        };
        for s in found {
            if !out.iter().any(|o| o.destination() == s.destination() && o.port == s.port) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

fn exec_fleet(targets: &[String], command: &[String], parallel: usize, timeout: u64, json: bool) -> Result<()> {
    use std::io::IsTerminal;
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let list = resolve_targets(&known, targets)?;
    let line = command.join(" ");
    let control = if cfg.servers.multiplex { paths::ssh_control_path() } else { None };
    let ssh = |s: &servers::Server| -> Vec<String> {
        let mut o = Vec::new();
        if let Some(c) = &control {
            o.extend([
                "-o".to_string(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", c.display()),
                "-o".into(),
                "ControlPersist=10m".into(),
            ]);
        }
        o.extend(s.ssh_options(&|j| servers::resolve(&known, j).map(|js| js.address())));
        o
    };
    let color = std::io::stdout().is_terminal();
    if !json {
        eprintln!("running `{line}` on {} server{}…", list.len(), if list.len() == 1 { "" } else { "s" });
    }
    let print = |o: &fleet::Outcome| {
        if !json {
            print!("{}", fleet::format_outcome(o, color));
        }
    };
    let results = fleet::run_all(list, &line, &ssh, parallel, std::time::Duration::from_secs(timeout), &print);
    let failed = results.iter().filter(|o| !o.ok()).count();
    if json {
        let v: Vec<serde_json::Value> = results
            .iter()
            .map(|o| {
                serde_json::json!({
                    "server": o.server, "code": o.code, "ok": o.ok(), "error": o.error,
                    "stdout": o.stdout, "stderr": o.stderr, "seconds": o.elapsed.as_secs_f64(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        eprintln!("{} ok, {failed} failed", results.len() - failed);
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
