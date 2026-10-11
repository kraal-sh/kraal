//! The server inventory: every machine you SSH to, with a short name and groups.
//!
//! `~/.config/kraal/servers.toml` holds the servers you saved (or that kraal discovered when you
//! ran `ssh` somewhere). Workspace files and `~/.ssh/config` contribute more, read-only. The
//! inventory names servers for the SSH picker, `kraal ssh`, `kraal cp` and file panes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::workspace::WorkspaceDef;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// ProxyJump: a server name from the inventory, or `user@host[:port]`, or a list of them
    /// (`a,b`); `none` turns off a jump host ~/.ssh/config would add. A jump host seen on an ssh
    /// command line is kept as ssh was given it (a `#` comment included: `none # x` is a host
    /// named `none`). A jump host wins over [`Server::proxy`], as in ssh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    /// A key, given to ssh as `-i` (which only takes a file that exists), or, written
    /// `IdentityFile=…`, as that option, where ssh expands tokens such as `%d` and `%h`: how an
    /// ssh command line seen running gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Keys ssh offers after [`Server::identity`], in order (`-i a -i b`), written the same way:
    /// ssh adds them up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more_identities: Vec<String>,
    /// ProxyCommand: how ssh reaches the server when it is not a plain connection or a jump host
    /// (seen on a command line; kraal also writes one for a jump host with its own key). `none`
    /// turns off a ProxyCommand or jump host ~/.ssh/config would add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Stage of the machine ("prod", "staging", "dev"). Decides whether kraal treats it as
    /// production; without it, the name, host and groups do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Where a known server comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Inventory,
    Workspace(String),
    SshConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub server: Server,
    pub source: Source,
}

impl Server {
    /// `user@host` (or `host`), as given to ssh.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// `user@host:port`: the port whenever one was given (22 too: without it, ~/.ssh/config
    /// decides), an IPv6 address in brackets (`ops@[fe80::1]`), so it reads back the same.
    pub fn address(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.bracketed_destination()),
            None => self.bracketed_destination(),
        }
    }

    /// `[user@]host[:port]` as ssh's `-J` reads it: the port whenever one was given, an IPv6
    /// address always in brackets (ssh refuses `ops@fe80::1`).
    pub fn jump_spec(&self) -> String {
        self.address()
    }

    fn bracketed_destination(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        match &self.user {
            Some(u) => format!("{u}@{host}"),
            None => host,
        }
    }

    /// Options for ssh/scp/rsync, without the destination. `jumps` finds a jump server (an
    /// inventory name or `user@host[:port]`) among the known ones.
    pub fn ssh_options(&self, jumps: &dyn Fn(&str) -> Option<Server>) -> Vec<String> {
        self.ssh_options_at(jumps, 0)
    }

    /// The options of an ssh reaching this server through `depth` jump hosts with their own
    /// key or jump host. Each of them is reached by an ssh of its own, run as the ProxyCommand of
    /// the one after it: ssh expands `%h` and `%p` in a ProxyCommand, so a token meant for the
    /// ssh `depth` levels in is written with its `%` doubled `depth` times (each ssh on the way
    /// halves them), and so is a `%` in a name or a path.
    fn ssh_options_at(&self, jumps: &dyn Fn(&str) -> Option<Server>, depth: usize) -> Vec<String> {
        let mut out = Vec::new();
        // Any port given, 22 too: ~/.ssh/config may set another one for this host.
        if let Some(p) = self.port {
            out.extend(["-p".to_string(), p.to_string()]);
        }
        match self.jump.as_deref().filter(|j| !is_none(j)) {
            // Only a single host is looked up (an inventory name stands for an address, and its
            // key or jump host needs an ssh of its own): a list of hosts goes to ssh as it was
            // given, as does a value that names no other address.
            Some(raw) => match jump_value(raw).filter(|v| !v.contains(',')).and_then(|v| Some((jumps(&v)?, v))) {
                // `-J` takes an address only: a jump host with its own key, jump host or
                // ProxyCommand is reached by an ssh of its own, which has them all.
                Some((js, _))
                    if depth < 4 && (js.identities().next().is_some() || js.jump.is_some() || js.proxy.is_some()) =>
                {
                    out.extend(["-o".to_string(), format!("ProxyCommand={}", js.proxy_command(jumps, depth + 1))]);
                }
                Some((js, v)) if js.jump_spec() != v => out.extend(["-J".to_string(), js.jump_spec()]),
                _ => out.extend(["-J".to_string(), raw.to_string()]),
            },
            // Turned off as given, in the order ssh needs: `ProxyJump=none` before a
            // ProxyCommand (the other way round, ssh would ignore the `none`).
            None => {
                if self.jump.is_some() {
                    out.extend(["-o".to_string(), "ProxyJump=none".to_string()]);
                }
                if let Some(proxy) = &self.proxy {
                    out.extend(["-o".to_string(), format!("ProxyCommand={proxy}")]);
                }
            }
        }
        for i in self.identities() {
            match identity_option(i) {
                Some(value) => out.extend(["-o".to_string(), format!("IdentityFile={}", setting_arg(value))]),
                None => out.extend(["-i".to_string(), paths::expand_tilde(i).display().to_string()]),
            }
        }
        out
    }

    /// The keys ssh offers, in order.
    pub fn identities(&self) -> impl Iterator<Item = &String> {
        self.identity.iter().chain(&self.more_identities)
    }

    /// The ProxyCommand that reaches this server as a jump host, as the ssh reading it must see
    /// it: `%h` and `%p` are the next host's, which `-W` asks this server to connect to (in
    /// brackets: an IPv6 address has colons). The options of the inner ssh are text inside this
    /// command, read by one more ssh before they take effect: every `%` in them is doubled.
    fn proxy_command(&self, jumps: &dyn Fn(&str) -> Option<Server>, depth: usize) -> String {
        let double = |a: &str| a.replace('%', "%%");
        let mut argv = vec!["ssh".to_string()];
        argv.extend(self.ssh_options_at(jumps, depth).iter().map(|a| double(a)));
        argv.extend(["-W".to_string(), "[%h]:%p".to_string(), double(&self.destination())]);
        crate::shell::join(&argv)
    }

    /// Whether two entries reach the same place the same way (user, host, port, jump, key):
    /// commands meant for one may run over a connection to the other. No port is not port 22:
    /// ~/.ssh/config may give that host another one.
    pub fn same_connection(&self, other: &Server) -> bool {
        // Host names are case-insensitive; user names are not.
        self.host.eq_ignore_ascii_case(&other.host)
            && self.user == other.user
            && self.port == other.port
            && self.jump == other.jump
            && self.identities().eq(other.identities())
            && self.proxy == other.proxy
    }

    /// Whether two entries name the same machine and account exactly as they were written: an
    /// entry without a user or a port leaves them to ~/.ssh/config, which may say otherwise
    /// than another entry that gives them. Two such entries are kept apart.
    fn same_address(&self, other: &Server) -> bool {
        self.host.eq_ignore_ascii_case(&other.host) && self.user == other.user && self.port == other.port
    }

    /// Whether two entries are likely the same machine and account (a user or a port left out
    /// matches any): enough to name a connection after a known server and take its groups,
    /// never to replace the connection.
    fn same_target(&self, other: &Server) -> bool {
        let users_match = match (&self.user, &other.user) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        self.host.eq_ignore_ascii_case(&other.host)
            && self.port.unwrap_or(22) == other.port.unwrap_or(22)
            && users_match
    }
}

/// Split `[ssh://][user@]host[:port]`. A bare IPv6 address keeps its colons.
pub fn parse_destination(s: &str) -> Option<(Option<String>, String, Option<u16>)> {
    let s = s.strip_prefix("ssh://").unwrap_or(s);
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, r)) => (Some(u.to_string()), r),
        None => (None, s),
    };
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let (h, after) = inner.split_once(']')?;
        let port = match after {
            "" => None,
            // `[::1]:22`, never `[::1]:bad` or `[::1]x`.
            _ => Some(after.strip_prefix(':')?.parse().ok()?),
        };
        (h.to_string(), port)
    } else if rest.matches(':').count() == 1 {
        let (h, p) = rest.split_once(':').unwrap();
        (h.to_string(), Some(p.parse().ok()?))
    } else {
        (rest.to_string(), None)
    };
    if host.is_empty() || host.contains(['/', ' ']) || user.as_deref() == Some("") {
        return None;
    }
    Some((user, host, port))
}

/// Options of ssh that take a value (from `man ssh`).
const SSH_VALUE_FLAGS: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// Whether an ssh command line (arguments after `ssh`) runs a command on the server instead
/// of an interactive shell.
pub fn ssh_runs_command(args: &[String]) -> bool {
    ssh_destination(args).is_some_and(|i| i + 1 < args.len())
}

/// Index of the destination in an ssh command line (arguments after `ssh`).
fn ssh_destination(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return (i + 1 < args.len()).then_some(i + 1);
        }
        if let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            for (pos, c) in flags.char_indices() {
                if SSH_VALUE_FLAGS.contains(c) {
                    if flags[pos + c.len_utf8()..].is_empty() {
                        i += 1;
                    }
                    break;
                }
            }
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

/// Read an ssh command line (arguments after `ssh`) into a server. `None` if there is no
/// destination (e.g. `ssh -V`), or ssh would refuse the line. As ssh does (OpenSSH's
/// readconf.c and ssh.c, checked with `ssh -G`): the first value given for a setting is the one
/// used (`-p 1 -p 2` is port 1), `-l` and `-p` win over the destination's user and port, keys
/// add up, and the route follows [`Route`].
pub fn parse_ssh_args(args: &[String]) -> Option<Server> {
    fn first<T>(slot: &mut Option<T>, value: Option<T>) {
        if slot.is_none() {
            *slot = value;
        }
    }
    let mut port = None;
    let mut user = None;
    let mut route = Route::default();
    let mut identities: Vec<String> = Vec::new();
    let mut hostname = None;
    let mut dest = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            dest = args.get(i + 1).cloned();
            break;
        }
        if let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            // Grouped flags (`-tt`, `-p22`, `-vp 22`): the first value flag takes the rest or the next argument.
            for (pos, c) in flags.char_indices() {
                if SSH_VALUE_FLAGS.contains(c) {
                    let inline = &flags[pos + c.len_utf8()..];
                    let value = if inline.is_empty() {
                        i += 1;
                        args.get(i).cloned()
                    } else {
                        Some(inline.to_string())
                    };
                    match (c, value) {
                        ('p', Some(v)) => first(&mut port, v.parse().ok()),
                        ('l', Some(v)) => first(&mut user, Some(v)),
                        ('J', Some(v)) => route.dash_j(jump_raw(&v)?)?,
                        ('i', Some(v)) => identities.push(v),
                        // `-o Key=value` (or `-o "Key value"`) settings that change where ssh goes.
                        // A blank line or a comment (`None` inside) changes nothing: ssh skips it.
                        ('o', Some(v)) => {
                            if let Some((key, rest, args)) = ssh_setting(&v)? {
                                // These take one argument; ssh refuses a line with more.
                                let one = || match args.as_slice() {
                                    [arg] => Some(arg.clone()),
                                    _ => None,
                                };
                                match key.as_str() {
                                    "port" => first(&mut port, one()?.parse().ok()),
                                    "user" => first(&mut user, Some(one()?)),
                                    // The rest of the line, as ssh's `parse_jump` reads it (a user
                                    // name may hold a space).
                                    "proxyjump" => {
                                        route.proxy_jump(jump_raw(rest.trim_start_matches([' ', '\t', '=']))?)
                                    }
                                    // The rest of the line, as it is: a command for the shell.
                                    "proxycommand" => {
                                        route.proxy_command(rest.trim_start_matches([' ', '\t', '=']).into())
                                    }
                                    "identityfile" => identities.push(format!("IdentityFile={}", one()?)),
                                    "hostname" => first(&mut hostname, Some(one()?)),
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                    break;
                }
            }
            i += 1;
            continue;
        }
        dest = Some(a.clone());
        break;
    }
    let (du, host, dport) = parse_destination(&dest?)?;
    let (jump, proxy) = route.settled();
    let mut identities = identities.into_iter();
    Some(Server {
        name: String::new(),
        host: hostname.unwrap_or(host),
        user: user.or(du),
        port: port.or(dport),
        jump,
        identity: identities.next(),
        more_identities: identities.collect(),
        proxy,
        groups: Vec::new(),
        env: None,
        note: None,
    })
}

/// An `-o` setting as ssh reads it (OpenSSH's readconf.c, with misc.c's `strdelim` and
/// `argv_split`): the keyword in lower case, then the rest of the line as text (a ProxyCommand
/// takes it whole) and as arguments, quotes and escapes removed. The keyword ends at a blank, a
/// quote or `=`, and one `=` may stand between it and the rest, blanks around it. `Some(None)`
/// for a line ssh skips: blank, a comment, or a keyword with a quote left open. `None` when ssh
/// refuses the line: nothing after the keyword, or a quote left open after it.
fn ssh_setting(line: &str) -> Option<Option<(String, &str, Vec<String>)>> {
    const BLANKS: [char; 4] = [' ', '\t', '\r', '\n'];
    // `strdelim`: a word, and what follows it once blanks and one `=` are skipped.
    fn word(s: &str) -> (String, Option<&str>) {
        let Some(at) = s.find([' ', '\t', '\r', '\n', '"', '=']) else { return (s.to_string(), None) };
        if s[at..].starts_with('"') {
            let quoted = &s[at + 1..];
            return match quoted.find('"') {
                Some(end) => {
                    (format!("{}{}", &s[..at], &quoted[..end]), Some(quoted[end + 1..].trim_start_matches(BLANKS)))
                }
                // ssh then ignores the line: no keyword it knows.
                None => (String::new(), None),
            };
        }
        let equals = s[at..].starts_with('=');
        let mut rest = s[at + 1..].trim_start_matches(BLANKS);
        if !equals && let Some(after) = rest.strip_prefix('=') {
            rest = after.trim_start_matches(BLANKS);
        }
        (s[..at].to_string(), Some(rest))
    }
    let line = line.trim_end_matches([' ', '\t', '\r', '\n', '\x0c']);
    let (mut key, mut rest) = word(line);
    if key.is_empty()
        && let Some(r) = rest
    {
        (key, rest) = word(r);
    }
    if key.is_empty() || key.starts_with('#') {
        return Some(None);
    }
    let rest = rest.map(|r| r.trim_start_matches(BLANKS)).filter(|r| !r.is_empty())?;
    Some(Some((key.to_ascii_lowercase(), rest, split_setting_args(rest)?)))
}

/// A ProxyJump value as ssh's `parse_jump` reads it (from `-J`, or the rest of a `ProxyJump`
/// line): a `#` and what follows it dropped, blanks after it trimmed. `None` when nothing is
/// left, which ssh refuses.
fn jump_value(raw: &str) -> Option<String> {
    let value = raw.split('#').next().unwrap_or(raw).trim_end_matches([' ', '\t', '\r', '\n']);
    (!value.is_empty()).then(|| value.to_string())
}

/// A ProxyJump value kept as ssh was given it, comment and all, when ssh takes it: ssh decides
/// on `none` before it drops a comment (`none # x` is a host named `none`), and a list of hosts
/// must reach ssh whole.
fn jump_raw(raw: &str) -> Option<String> {
    jump_value(raw).map(|_| raw.to_string())
}

/// The arguments of an ssh setting (OpenSSH's `argv_split`): split at blanks outside quotes,
/// single or double; a backslash escapes a quote, a backslash, or (outside quotes) a space; a
/// `#` where an argument would start ends the line. `None` for a quote left open.
fn split_setting_args(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        match chars.peek() {
            None | Some('#') => return Some(out),
            _ => {}
        }
        let (mut arg, mut quote) = (String::new(), None);
        while let Some(c) = chars.next() {
            match (c, quote) {
                ('\\', _) => match chars.peek() {
                    Some(&n) if n == '\'' || n == '"' || n == '\\' || (quote.is_none() && n == ' ') => {
                        arg.push(n);
                        chars.next();
                    }
                    _ => arg.push(c),
                },
                (' ' | '\t', None) => break,
                ('"' | '\'', None) => quote = Some(c),
                (c, Some(q)) if c == q => quote = None,
                (c, _) => arg.push(c),
            }
        }
        if quote.is_some() {
            return None;
        }
        out.push(arg);
    }
}

/// `value` as one argument of an ssh setting (`-o Key=value`), quoted as [`split_setting_args`]
/// reads it back when it holds blanks, quotes or backslashes, or starts with `#`.
fn setting_arg(value: &str) -> String {
    if !value.is_empty() && !value.starts_with('#') && !value.contains([' ', '\t', '"', '\'', '\\']) {
        return value.to_string();
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The value of a key written `IdentityFile=…` ([`Server::identity`]): passed to ssh as that
/// option, not as `-i`. The two differ: ssh drops a `-i` file that does not exist when it reads
/// the command line (and then offers its default keys), and takes an `IdentityFile` as it is,
/// tokens and all.
fn identity_option(key: &str) -> Option<&str> {
    const OPTION: &str = "IdentityFile=";
    key.get(..OPTION.len()).filter(|k| k.eq_ignore_ascii_case(OPTION)).map(|_| &key[OPTION.len()..])
}

/// Whether a ProxyJump or ProxyCommand value is `none`.
fn is_none(value: &str) -> bool {
    value.eq_ignore_ascii_case("none")
}

/// How ssh reaches a host, from its jump host and ProxyCommand settings, each `none` when
/// turned off, set as ssh sets them: a ProxyJump only while neither is set (a real one also
/// turns the ProxyCommand off), a ProxyCommand while none is set (`ProxyJump=none` leaves room
/// for one, as ssh does), and `-J` refused after either.
#[derive(Default)]
struct Route {
    jump: Option<String>,
    command: Option<String>,
}

impl Route {
    fn proxy_jump(&mut self, value: String) {
        if self.jump.is_none() && self.command.is_none() {
            self.set_jump(value);
        }
    }

    /// `None` when ssh would refuse the command line ("Only a single -J option is permitted",
    /// "Cannot specify -J with ProxyCommand").
    fn dash_j(&mut self, value: String) -> Option<()> {
        if self.jump.is_some() || self.command.is_some() {
            return None;
        }
        self.set_jump(value);
        Some(())
    }

    fn set_jump(&mut self, value: String) {
        if is_none(&value) {
            self.jump = Some("none".into());
        } else {
            self.jump = Some(value);
            self.command = Some("none".into());
        }
    }

    fn proxy_command(&mut self, value: String) {
        if self.command.is_none() {
            self.command = Some(if is_none(&value) { "none".into() } else { value });
        }
    }

    /// The jump host and ProxyCommand to record: a real jump host alone, else each as set (a
    /// `none` kept, so that what ~/.ssh/config would add stays off).
    fn settled(self) -> (Option<String>, Option<String>) {
        match self.jump {
            Some(j) if !is_none(&j) => (Some(j), None),
            jump => (jump, self.command),
        }
    }
}

/// A short name for a host: the first label of a DNS name, the whole IP otherwise.
pub fn short_name(host: &str) -> String {
    let is_ip = host.parse::<std::net::IpAddr>().is_ok();
    if is_ip { host.to_string() } else { host.split('.').next().unwrap_or(host).to_string() }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "server")]
    servers: Vec<Server>,
}

const HEADER: &str = "# kraal servers. Each [[server]] needs a name and a host; user, port, jump (ProxyJump),\n\
# identity, groups, env (\"prod\", \"staging\"…) and note are optional. kraal adds servers here\n\
# when you ssh somewhere new.\n\n";

pub struct Inventory {
    pub servers: Vec<Server>,
    path: PathBuf,
}

impl Inventory {
    pub fn path() -> PathBuf {
        paths::config_dir().join("servers.toml")
    }

    pub fn empty() -> Inventory {
        Inventory { servers: Vec::new(), path: Self::path() }
    }

    pub fn load() -> Result<Inventory> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Inventory> {
        let servers: Vec<Server> = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<File>(&text).with_context(|| format!("parsing {}", path.display()))?.servers,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        // A name picks one server: two servers with one name would make it pick either.
        for (i, s) in servers.iter().enumerate() {
            if servers[..i].iter().any(|o| o.name == s.name) {
                bail!("{}: two servers are named {:?}; rename one", path.display(), s.name);
            }
        }
        Ok(Inventory { servers, path: path.to_path_buf() })
    }

    fn text(&self) -> Result<String> {
        Ok(format!("{HEADER}{}", toml::to_string_pretty(&File { servers: self.servers.clone() })?))
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        paths::write_atomic(&self.path, self.text()?.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Load the inventory, change it with `f` and save it if it changed, with no other kraal
    /// process updating it in between.
    pub fn update<T>(f: impl FnOnce(&mut Inventory) -> Result<T>) -> Result<T> {
        Self::update_at(&Self::path(), f)
    }

    pub fn update_at<T>(path: &Path, f: impl FnOnce(&mut Inventory) -> Result<T>) -> Result<T> {
        paths::locked(path, || {
            let mut inv = Self::load_from(path)?;
            let before = inv.text()?;
            let out = f(&mut inv)?;
            if inv.text()? != before {
                inv.save()?;
            }
            Ok(out)
        })?
    }

    pub fn get(&self, name: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// An existing entry for the same machine and account, as written (`box:22` is not `box`,
    /// whose port ~/.ssh/config may set).
    pub fn find_target(&self, s: &Server) -> Option<&Server> {
        self.servers.iter().find(|x| x.same_address(s))
    }

    /// Add a server, choosing a free name if needed. Returns the stored entry's name.
    pub fn add(&mut self, mut server: Server) -> Result<String> {
        if server.host.is_empty() {
            bail!("a server needs a host");
        }
        if let Some(existing) = self.find_target(&server) {
            return Ok(existing.name.clone());
        }
        if server.name.is_empty() {
            server.name = short_name(&server.host);
        }
        if !valid_name(&server.name) {
            bail!("invalid server name {:?} (letters, digits, - _ . only)", server.name);
        }
        let base = server.name.clone();
        let mut n = 2;
        while self.get(&server.name).is_some() {
            server.name = match &server.user {
                Some(u) if n == 2 => format!("{base}-{u}"),
                _ => format!("{base}-{n}"),
            };
            n += 1;
        }
        let name = server.name.clone();
        self.servers.push(server);
        Ok(name)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != name);
        before != self.servers.len()
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Servers from workspace definitions: every `ssh` command pane, named after the pane.
pub fn from_workspaces(defs: &[WorkspaceDef]) -> Vec<Known> {
    let mut out: Vec<Known> = Vec::new();
    for def in defs {
        for p in def.panes() {
            let Some(cmd) = &p.command else { continue };
            if cmd.rsplit('/').next() != Some("ssh") {
                continue;
            }
            let Some(mut s) = parse_ssh_args(&p.args) else { continue };
            s.name = p.name.clone().filter(|n| valid_name(n)).unwrap_or_else(|| short_name(&s.host));
            s.groups = vec![def.name.clone()];
            if let Some(k) = out.iter_mut().find(|k| k.server.same_address(&s) && k.server.same_connection(&s)) {
                if !k.server.groups.contains(&def.name) {
                    k.server.groups.push(def.name.clone());
                }
                continue;
            }
            out.push(Known { server: s, source: Source::Workspace(def.name.clone()) });
        }
    }
    out
}

/// Every server kraal knows: the inventory first, then workspaces, then `~/.ssh/config`
/// aliases. Duplicates (the same connection, written the same way) keep the first entry; a
/// workspace pane that connects otherwise (another user, port, key or jump host) keeps its own
/// entry, so its name opens what it opens. Names stay unique.
pub fn known(inv: &Inventory, defs: &[WorkspaceDef], ssh_hosts: &[String]) -> Vec<Known> {
    let mut out: Vec<Known> =
        inv.servers.iter().map(|s| Known { server: s.clone(), source: Source::Inventory }).collect();
    for k in from_workspaces(defs) {
        if let Some(existing) =
            out.iter_mut().find(|e| e.server.same_address(&k.server) && e.server.same_connection(&k.server))
        {
            for g in &k.server.groups {
                if existing.source != Source::Inventory && !existing.server.groups.contains(g) {
                    existing.server.groups.push(g.clone());
                }
            }
            continue;
        }
        let mut k = k;
        if out.iter().any(|e| e.server.name == k.server.name) {
            let base = format!("{}-{}", k.server.name, k.server.groups.first().cloned().unwrap_or_default());
            k.server.name = base.clone();
            let mut n = 2;
            while out.iter().any(|e| e.server.name == k.server.name) {
                k.server.name = format!("{base}-{n}");
                n += 1;
            }
        }
        out.push(k);
    }
    for h in ssh_hosts {
        if out.iter().any(|e| e.server.name == *h || e.server.host == *h) {
            continue;
        }
        out.push(Known {
            server: Server {
                name: h.clone(),
                host: h.clone(),
                groups: vec!["ssh-config".into()],
                ..Default::default()
            },
            source: Source::SshConfig,
        });
    }
    out
}

/// Look a server up by name, then by `user@host[:port]`.
/// ssh options (without the destination) to reach `s`: one shared connection per server when
/// `share` (kraal's commands then reuse what is open), then the server's own options, its jump
/// host resolved among `known`.
pub fn ssh_args(s: &Server, known: &[Known], share: bool) -> Vec<String> {
    let mut out = Vec::new();
    if share && let Some(dir) = paths::ssh_control_dir() {
        // ssh's `%C` covers the destination, not the key or the jump host: a connection opened
        // with one key must not carry the commands meant for another.
        let path = dir.join(format!("ssh-{}-%C", connection_key(s)));
        out.extend([
            "-o".to_string(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", path.display()),
            "-o".into(),
            "ControlPersist=10m".into(),
        ]);
    }
    out.extend(s.ssh_options(&|j| resolve(known, j)));
    out
}

/// 16 hex digits for what of a connection ssh's `%C` leaves out (key, jump host, ProxyCommand).
fn connection_key(s: &Server) -> String {
    let keys: Vec<&str> = s.identities().map(String::as_str).collect();
    let text = [keys.join("\n").as_str(), s.jump.as_deref().unwrap_or(""), s.proxy.as_deref().unwrap_or("")].join("\0");
    format!("{:016x}", crate::text::fnv64(&text))
}

pub fn resolve(known: &[Known], spec: &str) -> Option<Server> {
    if let Some(k) = known.iter().find(|k| k.server.name == spec) {
        return Some(k.server.clone());
    }
    let (user, host, port) = parse_destination(spec)?;
    let probe = Server { host, user, port, ..Default::default() };
    if let Some(k) =
        known.iter().find(|k| k.server.same_target(&probe) && probe.user.is_some() == k.server.user.is_some())
    {
        let mut s = k.server.clone();
        // `host:22` asked for port 22 even if the entry leaves it to ~/.ssh/config.
        s.port = s.port.or(probe.port);
        return Some(s);
    }
    Some(Server { name: short_name(&probe.host), ..probe })
}

/// Whether `spec` names a server kraal knows (by name, ssh alias, or address).
pub fn is_known(known: &[Known], s: &Server) -> bool {
    known.iter().any(|k| k.server.same_target(s) || (s.user.is_none() && s.port.is_none() && k.server.name == s.host))
}

/// All group names, sorted, with their member count.
pub fn groups(known: &[Known]) -> Vec<(String, usize)> {
    let mut map = std::collections::BTreeMap::<String, usize>::new();
    for k in known {
        for g in &k.server.groups {
            *map.entry(g.clone()).or_default() += 1;
        }
    }
    map.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn destinations() {
        assert_eq!(parse_destination("deploy@host.lan"), Some((Some("deploy".into()), "host.lan".into(), None)));
        assert_eq!(
            parse_destination("ssh://root@10.0.0.1:2222"),
            Some((Some("root".into()), "10.0.0.1".into(), Some(2222)))
        );
        assert_eq!(parse_destination("[::1]:22"), Some((None, "::1".into(), Some(22))));
        assert_eq!(parse_destination("fe80::1"), Some((None, "fe80::1".into(), None)));
        assert_eq!(parse_destination("host:notaport"), None);
        assert_eq!(parse_destination("@host"), None);
    }

    #[test]
    fn ssh_command_lines() {
        let s = parse_ssh_args(&argv("-p 2222 root@pve-main.example.net")).unwrap();
        assert_eq!((s.user.as_deref(), s.host.as_str(), s.port), (Some("root"), "pve-main.example.net", Some(2222)));
        let s = parse_ssh_args(&argv("-t -J bastion -l admin web uptime")).unwrap();
        assert_eq!((s.user.as_deref(), s.host.as_str(), s.jump.as_deref()), (Some("admin"), "web", Some("bastion")));
        let s = parse_ssh_args(&argv("-vp2222 -i ~/.ssh/k db")).unwrap();
        assert_eq!((s.port, s.identity.as_deref()), (Some(2222), Some("~/.ssh/k")));
        let s = parse_ssh_args(&argv("-o StrictHostKeyChecking=no x@y")).unwrap();
        assert_eq!(s.host, "y");
        assert!(parse_ssh_args(&argv("-V")).is_none());
        let s = parse_ssh_args(&argv("-o Port=2200 -o User=ops -oProxyJump=bastion -o HostName=10.0.0.9 web")).unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.user.as_deref(), s.jump.as_deref()),
            ("10.0.0.9", Some(2200), Some("ops"), Some("bastion"))
        );
        // What OpenSSH 10.6 uses (`ssh -G`) when settings repeat or conflict: the first value,
        // `-l`/`-p` before the destination's, and the first of a jump host and a ProxyCommand.
        let route = |line: &str| {
            let s = parse_ssh_args(&argv(line)).unwrap();
            (s.jump, s.proxy)
        };
        assert_eq!(route("-J bastion -o ProxyCommand=backup 10.0.0.5"), (Some("bastion".into()), None));
        assert_eq!(route("-o ProxyCommand=backup -o ProxyJump=bastion 10.0.0.5"), (None, Some("backup".into())));
        assert_eq!(route("-o ProxyCommand=first -o ProxyCommand=second h"), (None, Some("first".into())));
        assert_eq!(route("-o ProxyJump=a -o ProxyJump=b h"), (Some("a".into()), None));
        assert_eq!(route("-o ProxyCommand=none -o ProxyJump=a h"), (None, Some("none".into())));
        // `none` turns a setting off and is kept: what ~/.ssh/config would add stays off.
        assert_eq!(route("-J none h"), (Some("none".into()), None));
        assert_eq!(route("-o ProxyJump=none -o ProxyCommand=x h"), (Some("none".into()), Some("x".into())));
        assert_eq!(route("-o ProxyJump=none -o ProxyJump=a h"), (Some("none".into()), None));
        // Lines ssh refuses connect nowhere.
        assert!(parse_ssh_args(&argv("-J none -J b h")).is_none());
        assert!(parse_ssh_args(&argv("-o ProxyCommand=x -J b h")).is_none());
        // Keys add up, in order, each kept as it was given: an `IdentityFile` stays one (ssh
        // expands its tokens), a `-i` stays one.
        let s = parse_ssh_args(&argv("-o IdentityFile=%d/k -i b -i c h")).unwrap();
        assert_eq!(s.identities().collect::<Vec<_>>(), ["IdentityFile=%d/k", "b", "c"]);
        assert_eq!(s.ssh_options(&|_| None), argv("-o IdentityFile=%d/k -i b -i c"));
        let s = parse_ssh_args(&argv("-p 1 -p 2 -o Port=3 -l a -o User=b -o HostName=x -o HostName=y c@h")).unwrap();
        assert_eq!((s.port, s.user.as_deref(), s.host.as_str()), (Some(1), Some("a"), "x"));
        let s = parse_ssh_args(&argv("-o Port=7 -i k1 -i k2 ssh://u@h:6")).unwrap();
        assert_eq!((s.port, s.user.as_deref(), s.identity.as_deref()), (Some(7), Some("u"), Some("k1")));
        let alice = Server { host: "Web.lan".into(), user: Some("Alice".into()), ..Default::default() };
        let lower = Server { host: "web.lan".into(), user: Some("alice".into()), ..Default::default() };
        assert!(!alice.same_connection(&lower), "user names are case-sensitive");
        assert!(alice.same_connection(&Server { host: "web.LAN".into(), ..alice.clone() }), "host names are not");
        assert!(!ssh_runs_command(&argv("-p 2222 -o ControlMaster=auto root@web")));
        assert!(ssh_runs_command(&argv("-t -p2222 root@web journalctl -f")));
        assert!(ssh_runs_command(&argv("-- web uptime")));
        assert!(!ssh_runs_command(&argv("-V")));
    }

    #[test]
    fn inventory_add_dedupes_and_names() {
        let dir = std::env::temp_dir().join(format!("kraal-servers-{}", std::process::id()));
        let path = dir.join("servers.toml");
        let mut inv = Inventory::load_from(&path).unwrap();
        assert!(inv.servers.is_empty());
        let a = parse_ssh_args(&argv("deploy@edge-proxy.lan")).unwrap();
        assert_eq!(inv.add(a.clone()).unwrap(), "edge-proxy");
        assert_eq!(inv.add(a).unwrap(), "edge-proxy", "same target is not added twice");
        let b = parse_ssh_args(&argv("root@edge-proxy.lan")).unwrap();
        assert_eq!(inv.add(b).unwrap(), "edge-proxy-root");
        inv.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# kraal servers"));
        let mut again = Inventory::load_from(&path).unwrap();
        assert_eq!(again.servers.len(), 2);
        assert!(again.remove("edge-proxy-root"));
        assert!(!again.remove("edge-proxy-root"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn known_merges_sources() {
        let def = crate::workspace::parse_workspace(
            "fleet",
            r#"layout {
    pane split_direction="vertical" {
        pane name="edge-proxy" command="ssh" { args "deploy@edge-proxy.lan"; }
        pane name="db" command="ssh" { args "-p" "2222" "deploy@db.lan"; }
        pane name="local"
    }
}"#,
        )
        .unwrap();
        let mut inv = Inventory { servers: Vec::new(), path: PathBuf::from("/nonexistent") };
        inv.add(Server {
            name: "proxy".into(),
            host: "edge-proxy.lan".into(),
            user: Some("deploy".into()),
            ..Default::default()
        })
        .unwrap();
        let k = known(&inv, &[def], &["bastion".to_string(), "db".to_string()]);
        let names: Vec<&str> = k.iter().map(|k| k.server.name.as_str()).collect();
        assert_eq!(names, vec!["proxy", "db", "bastion"]);
        assert_eq!(k[1].server.port, Some(2222));
        assert_eq!(k[1].server.groups, vec!["fleet".to_string()]);
        assert_eq!(resolve(&k, "db").unwrap().host, "db.lan");
        assert_eq!(resolve(&k, "deploy@edge-proxy.lan").unwrap().name, "proxy");
        assert_eq!(resolve(&k, "me@new.example.org").unwrap().name, "new");
        assert!(is_known(&k, &parse_ssh_args(&argv("bastion")).unwrap()));
        assert!(!is_known(&k, &parse_ssh_args(&argv("me@other")).unwrap()));
        assert_eq!(groups(&k), vec![("fleet".to_string(), 1), ("ssh-config".to_string(), 1)]);
    }

    #[test]
    fn ssh_options() {
        let s = Server {
            name: "x".into(),
            host: "h".into(),
            port: Some(2222),
            jump: Some("bastion".into()),
            ..Default::default()
        };
        let bastion = Server {
            name: "bastion".into(),
            host: "b.lan".into(),
            user: Some("admin".into()),
            port: Some(22),
            ..Default::default()
        };
        let plain = |j: &str| (j == "bastion").then(|| bastion.clone());
        let opts = s.ssh_options(&plain);
        let other_port = Server { port: Some(22), ..s.clone() };
        let no_jump = Server { jump: None, ..s.clone() };
        assert!(s.same_connection(&s.clone()));
        assert!(!s.same_connection(&other_port) && !s.same_connection(&no_jump));
        let unset = Server { port: None, ..other_port.clone() };
        assert!(!unset.same_connection(&other_port), "no port is whatever ssh's config says, maybe not 22");
        assert_eq!(opts, argv("-p 2222 -J admin@b.lan:22"));
        assert_eq!(s.address(), "h:2222");
        // A port given is a port used, 22 too: ~/.ssh/config may say another for this host.
        assert_eq!(other_port.ssh_options(&plain)[..2], argv("-p 22"));
        assert!(Server { port: None, jump: None, ..s.clone() }.ssh_options(&plain).is_empty());
        // A jump host with its own key, behind its own jump host: a nested ssh carries them.
        let outer = Server { name: "outer".into(), host: "gw.example.org".into(), ..Default::default() };
        let keyed = Server { identity: Some("/k/bastion".into()), jump: Some("outer".into()), ..bastion.clone() };
        let chain = |j: &str| match j {
            "bastion" => Some(keyed.clone()),
            "outer" => Some(outer.clone()),
            _ => None,
        };
        let opts = s.ssh_options(&chain);
        assert_eq!(
            opts,
            vec![
                "-p".to_string(),
                "2222".into(),
                "-o".into(),
                "ProxyCommand=ssh -p 22 -J gw.example.org -i /k/bastion -W '[%h]:%p' admin@b.lan".into()
            ]
        );
        // Two jump hosts with their own key: the inner `%h` is the bastion's, so its `%` is
        // doubled (the outer ssh halves it), and so is a `%` in a path.
        let gw = Server { identity: Some("/k/gw%1".into()), ..outer.clone() };
        let keyed_chain = |j: &str| match j {
            "bastion" => Some(keyed.clone()),
            "outer" => Some(gw.clone()),
            _ => None,
        };
        let opts = s.ssh_options(&keyed_chain);
        assert_eq!(
            opts[3],
            "ProxyCommand=ssh -p 22 -o 'ProxyCommand=ssh -i /k/gw%%%%1 -W '\\''[%%h]:%%p'\\'' gw.example.org' \
             -i /k/bastion -W '[%h]:%p' admin@b.lan"
        );
        // A jump host whose keys are all in `more_identities` needs its own ssh just the same.
        let more_keyed = Server { more_identities: vec!["/k/second".into()], ..bastion.clone() };
        let opts = s.ssh_options(&|j| (j == "bastion").then(|| more_keyed.clone()));
        assert_eq!(
            opts[2..],
            ["-o".to_string(), "ProxyCommand=ssh -p 22 -i /k/second -W '[%h]:%p' admin@b.lan".into()]
        );
        // A loop of jump hosts ends.
        let looping = Server { jump: Some("bastion".into()), identity: Some("/k".into()), ..bastion.clone() };
        assert!(!s.ssh_options(&|_| Some(looping.clone())).is_empty());
        // IPv6 addresses take brackets where a port follows, and always for `-J`.
        let v6 = Server { host: "fe80::1".into(), user: Some("ops".into()), port: Some(2200), ..Default::default() };
        assert_eq!(v6.jump_spec(), "ops@[fe80::1]:2200");
        assert_eq!(v6.address(), "ops@[fe80::1]:2200");
        assert_eq!(Server { port: None, ..v6.clone() }.jump_spec(), "ops@[fe80::1]");
        // An IPv6 target behind a jump host with a key: `-W` takes it in brackets too.
        let v6_target = Server { host: "::1".into(), jump: Some("bastion".into()), ..Default::default() };
        assert!(v6_target.ssh_options(&chain)[1].contains("-W '[%h]:%p'"));
    }

    #[test]
    fn what_kraal_writes_for_a_route_reads_back_as_that_route() {
        let bastion = Server {
            name: "bastion".into(),
            host: "b.lan".into(),
            user: Some("admin".into()),
            identity: Some("/k/bastion".into()),
            ..Default::default()
        };
        let s =
            Server { name: "web".into(), host: "10.0.0.5".into(), jump: Some("bastion".into()), ..Default::default() };
        let opts = s.ssh_options(&|j| (j == "bastion").then(|| bastion.clone()));
        let mut line = opts.clone();
        line.push(s.destination());
        // An ssh seen running with that command line goes the same way, not straight to the
        // address (which may be another machine from here).
        let seen = parse_ssh_args(&line).unwrap();
        assert!(seen.proxy.is_some() && seen.jump.is_none());
        assert_eq!(seen.ssh_options(&|_| None), opts);
        assert!(!seen.same_connection(&Server { proxy: None, ..seen.clone() }));
        assert_eq!(parse_ssh_args(&argv("-o ProxyCommand=none web")).unwrap().proxy.as_deref(), Some("none"));
    }

    /// Whatever ssh command line kraal saw, the options it writes back make ssh go the same way
    /// with the same keys, ~/.ssh/config included: checked with the local `ssh -G`.
    #[cfg(unix)]
    #[test]
    fn observed_routes_and_keys_are_what_ssh_uses() {
        let ssh_g = |config: &Path, args: &[String]| -> Option<Vec<String>> {
            let out = std::process::Command::new("ssh").arg("-G").arg("-F").arg(config).args(args).output().ok()?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            Some(
                text.lines()
                    .filter(|l| ["proxyjump ", "proxycommand ", "identityfile "].iter().any(|k| l.starts_with(k)))
                    .map(String::from)
                    .collect(),
            )
        };
        let dir = std::env::temp_dir().join(format!("kraal-ssh-g-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, prod) = (dir.join("old"), dir.join("prod"));
        for k in [&old, &prod] {
            std::fs::write(k, "").unwrap();
        }
        let jump_config = dir.join("jump");
        std::fs::write(&jump_config, "Host *\n  ProxyJump bastion\n").unwrap();
        let command_config = dir.join("command");
        std::fs::write(&command_config, "Host *\n  ProxyCommand ssh cfg -W %h:%p\n").unwrap();
        let lines: Vec<Vec<String>> = vec![
            vec!["-J".into(), "none".into()],
            vec!["-o".into(), "ProxyJump=none".into()],
            vec!["-o".into(), "ProxyJump=none".into(), "-o".into(), "ProxyCommand=ssh backup -W %h:%p".into()],
            vec!["-o".into(), "ProxyCommand=none".into()],
            vec!["-o".into(), "ProxyCommand=none".into(), "-o".into(), "ProxyJump=a".into()],
            vec!["-J".into(), "b".into(), "-o".into(), "ProxyCommand=ssh a -W %h:%p".into()],
            vec!["-o".into(), "ProxyCommand=ssh a -W %h:%p".into(), "-o".into(), "ProxyJump=c".into()],
            vec!["-i".into(), old.display().to_string(), "-i".into(), prod.display().to_string()],
            vec!["-o".into(), format!("IdentityFile={}", old.display()), "-i".into(), prod.display().to_string()],
            // Tokens in an IdentityFile are ssh's to expand; a `-i` with them names no file.
            vec!["-i".into(), old.display().to_string(), "-o".into(), "IdentityFile=%d/.ssh/%h".into()],
            vec!["-o".into(), "IdentityFile %d/.ssh/%h".into()],
            // Written as ssh reads a configuration line: `=` with blanks around it, tabs, blanks
            // first, quotes and escapes.
            vec!["-i".into(), old.display().to_string(), "-o".into(), "IdentityFile = %d/.ssh/%h".into()],
            vec!["-o".into(), "IdentityFile\t%d/.ssh/%h".into()],
            vec!["-o".into(), "  identityfile =\t%d/k".into()],
            vec!["-o".into(), format!("IdentityFile \"{}/a b\"", dir.display())],
            vec!["-o".into(), format!("IdentityFile={}/q\\\"uote", dir.display())],
            vec!["-o".into(), "IdentityFile '#hash'".into()],
            vec!["-o".into(), "ProxyCommand = ssh a -W %h:%p".into()],
            vec!["-o".into(), "ProxyJump\tb".into()],
            vec!["-i".into(), "%d/.ssh/%h".into()],
            // A `-i` file that does not exist is dropped, and ssh offers its default keys.
            vec!["-i".into(), dir.join("missing").display().to_string()],
            // A jump host is the rest of its line, a user name may hold a space, and a `#` ends it.
            vec!["-o".into(), "ProxyJump=John Doe@bastion".into()],
            vec!["-J".into(), "John Doe@bastion".into()],
            vec!["-o".into(), "ProxyJump=a b@c,d e@f".into()],
            vec!["-o".into(), "ProxyJump bastion # comment".into()],
            // A list of jump hosts goes to ssh whole, and `none` turns the jump host off only as
            // the whole value: with a comment or a blank after it, it is a host named `none`.
            vec!["-J".into(), "outer:2222,inner:2200 # private network".into()],
            vec!["-J".into(), "outer:2222,inner:2200 ".into()],
            vec!["-o".into(), "ProxyJump=outer:2222,inner:2200 # private".into()],
            vec!["-J".into(), "none # gateway".into()],
            vec!["-J".into(), "none ".into()],
            vec!["-J".into(), "NONE".into()],
            vec!["-o".into(), "ProxyJump=none # c".into()],
            vec!["-o".into(), "ProxyJump=None".into()],
            vec!["-o".into(), "ProxyJump  =  bastion".into()],
            vec!["-o".into(), "=ProxyJump x".into()],
            // Lines ssh skips change nothing, and the rest of the command line still counts.
            vec!["-o".into(), "#disabled".into(), "-J".into(), "b".into()],
            vec!["-o".into(), String::new(), "-o".into(), "ProxyJump=c".into()],
            vec!["-o".into(), "   ".into(), "-i".into(), prod.display().to_string()],
            vec!["-o".into(), " # spaced comment".into()],
            vec!["-o".into(), "\"unterminated".into(), "-J".into(), "b".into()],
        ];
        for config in [&jump_config, &command_config, &dir.join("empty")] {
            let _ = std::fs::write(dir.join("empty"), "");
            for line in &lines {
                let mut seen_line = line.clone();
                seen_line.push("10.0.0.5".into());
                let Some(expected) = ssh_g(config, &seen_line) else { return };
                let seen = parse_ssh_args(&seen_line).unwrap();
                // With no server known, and looked up as kraal does (an address it does not
                // know stands for itself).
                for jumps in [&(|_: &str| None) as &dyn Fn(&str) -> Option<Server>, &|j: &str| resolve(&[], j)] {
                    let mut again = seen.ssh_options(jumps);
                    again.push(seen.destination());
                    assert_eq!(
                        ssh_g(config, &again),
                        Some(expected.clone()),
                        "{} with {}: {again:?}",
                        line.join(" "),
                        config.display()
                    );
                }
            }
        }
        // Lines ssh refuses are no connection kraal would make again either.
        for bad in [
            "IdentityFile=a b",
            "IdentityFile",
            "IdentityFile \"open",
            "IdentityFile=#k",
            "Port 1 2",
            "ProxyJump=#x",
            "ProxyJump",
            "ProxyJump=   ",
            "ProxyJump=b\"x",
        ] {
            let line: Vec<String> = vec!["-o".into(), bad.into(), "10.0.0.5".into()];
            let refused = std::process::Command::new("ssh")
                .arg("-G")
                .args(["-F", "/dev/null"])
                .args(&line)
                .output()
                .is_ok_and(|o| !o.status.success());
            assert!(refused, "ssh takes {bad:?}");
            assert!(parse_ssh_args(&line).is_none(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shared_connections_are_per_key() {
        let a = Server { name: "a".into(), host: "h".into(), identity: Some("/k/one".into()), ..Default::default() };
        let b = Server { identity: Some("/k/two".into()), ..a.clone() };
        assert_ne!(connection_key(&a), connection_key(&b));
        assert_eq!(connection_key(&a), connection_key(&a.clone()));
        assert_eq!(connection_key(&a).len(), 16);
        let behind = Server { jump: Some("bastion".into()), ..a.clone() };
        assert_ne!(connection_key(&a), connection_key(&behind));
        let proxied = Server { proxy: Some("nc %h %p".into()), ..a.clone() };
        assert_ne!(connection_key(&a), connection_key(&proxied));
        // Two keys whose 32-bit digests were equal.
        let k1 = Server { identity: Some("/keys/key79120".into()), ..a.clone() };
        let k2 = Server { identity: Some("/keys/key151242".into()), ..a.clone() };
        assert_ne!(connection_key(&k1), connection_key(&k2));
    }

    #[test]
    fn names_and_ports_stay_what_was_asked() {
        // `host:22` keeps its explicit port even when the known entry leaves it to ssh's config.
        let k = vec![Known {
            server: Server { name: "box".into(), host: "box".into(), ..Default::default() },
            source: Source::SshConfig,
        }];
        let r = resolve(&k, "box:22").unwrap();
        assert_eq!((r.name.as_str(), r.port), ("box", Some(22)));
        assert_eq!(resolve(&k, "box").unwrap().port, None);
        assert_eq!(parse_destination("[::1]:bad"), None);
        assert_eq!(parse_destination("[::1]x"), None);
        assert_eq!(parse_destination("[::1]"), Some((None, "::1".into(), None)));
        // Generated names never collide with existing ones.
        let def = crate::workspace::parse_workspace(
            "fleet",
            "layout {\n    pane name=\"db\" command=\"ssh\" { args \"deploy@db.other\"; }\n}\n",
        )
        .unwrap();
        let mut inv = Inventory { servers: Vec::new(), path: PathBuf::from("/nonexistent") };
        for (name, host) in [("db", "db.lan"), ("db-fleet", "db-fleet.lan")] {
            inv.servers.push(Server { name: name.into(), host: host.into(), ..Default::default() });
        }
        let k = known(&inv, &[def], &[]);
        let names: Vec<&str> = k.iter().map(|k| k.server.name.as_str()).collect();
        assert_eq!(names, vec!["db", "db-fleet", "db-fleet-2"]);
        assert_eq!(resolve(&k, "db-fleet-2").unwrap().host, "db.other");
        // `box:22` is not `box`, whose port ~/.ssh/config may set: two entries.
        let mut inv = Inventory { servers: Vec::new(), path: PathBuf::from("/nonexistent") };
        assert_eq!(inv.add(parse_ssh_args(&argv("box")).unwrap()).unwrap(), "box");
        assert_eq!(inv.add(parse_ssh_args(&argv("-p 22 box")).unwrap()).unwrap(), "box-2");
        assert_eq!(inv.get("box-2").unwrap().port, Some(22));
        // A workspace pane connecting with another account keeps its own entry, and its name.
        let def = crate::workspace::parse_workspace(
            "ops",
            "layout {\n    pane name=\"web\" command=\"ssh\" { args \"deploy@web.lan\"; }\n}\n",
        )
        .unwrap();
        let mut inv = Inventory { servers: Vec::new(), path: PathBuf::from("/nonexistent") };
        inv.servers.push(Server { name: "web".into(), host: "web.lan".into(), ..Default::default() });
        let k = known(&inv, &[def], &[]);
        let r = resolve(&k, "web-ops").unwrap();
        assert_eq!((r.host.as_str(), r.user.as_deref()), ("web.lan", Some("deploy")));
    }

    #[test]
    fn an_inventory_with_two_servers_of_one_name_is_refused() {
        let dir = std::env::temp_dir().join(format!("kraal-servers-dup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.toml");
        std::fs::write(&path, "[[server]]\nname = \"db\"\nhost = \"a\"\n\n[[server]]\nname = \"db\"\nhost = \"b\"\n")
            .unwrap();
        let err = Inventory::load_from(&path).err().unwrap().to_string();
        assert!(err.contains("two servers are named \"db\""), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
