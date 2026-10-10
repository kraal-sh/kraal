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
    /// ProxyJump: a server name from the inventory, or `user@host[:port]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
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

    /// `user@host:port` for display.
    pub fn address(&self) -> String {
        match self.port {
            Some(p) if p != 22 => format!("{}:{p}", self.destination()),
            _ => self.destination(),
        }
    }

    /// Options for ssh/scp/rsync, without the destination. `jump_resolver` turns a jump server
    /// name into its `user@host:port`.
    pub fn ssh_options(&self, jump_resolver: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(p) = self.port.filter(|p| *p != 22) {
            out.extend(["-p".to_string(), p.to_string()]);
        }
        if let Some(j) = &self.jump {
            out.extend(["-J".to_string(), jump_resolver(j).unwrap_or_else(|| j.clone())]);
        }
        if let Some(i) = &self.identity {
            out.extend(["-i".to_string(), paths::expand_tilde(i).display().to_string()]);
        }
        out
    }

    /// Identity used to decide whether two entries are the same machine and account.
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
        (h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
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

/// Read an ssh command line (arguments after `ssh`) into a server. `None` if there is no
/// destination (e.g. `ssh -V`).
pub fn parse_ssh_args(args: &[String]) -> Option<Server> {
    let mut port = None;
    let mut user = None;
    let mut jump = None;
    let mut identity = None;
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
                        ('p', Some(v)) => port = v.parse().ok(),
                        ('l', Some(v)) => user = Some(v),
                        ('J', Some(v)) => jump = Some(v),
                        ('i', Some(v)) => identity = Some(v),
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
    Some(Server {
        name: String::new(),
        host,
        user: du.or(user),
        port: dport.or(port),
        jump,
        identity,
        groups: Vec::new(),
        note: None,
    })
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
# identity, groups and note are optional. kraal adds servers here when you ssh somewhere new.\n\n";

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
        let servers = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<File>(&text).with_context(|| format!("parsing {}", path.display()))?.servers,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Inventory { servers, path: path.to_path_buf() })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = toml::to_string_pretty(&File { servers: self.servers.clone() })?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("{HEADER}{body}"))?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// An existing entry for the same machine and account.
    pub fn find_target(&self, s: &Server) -> Option<&Server> {
        self.servers.iter().find(|x| x.same_target(s))
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
            if let Some(k) = out.iter_mut().find(|k| k.server.same_target(&s)) {
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
/// aliases. Duplicates (same host, port and user) keep the first entry; names stay unique.
pub fn known(inv: &Inventory, defs: &[WorkspaceDef], ssh_hosts: &[String]) -> Vec<Known> {
    let mut out: Vec<Known> =
        inv.servers.iter().map(|s| Known { server: s.clone(), source: Source::Inventory }).collect();
    for k in from_workspaces(defs) {
        if let Some(existing) = out.iter_mut().find(|e| e.server.same_target(&k.server)) {
            for g in &k.server.groups {
                if existing.source != Source::Inventory && !existing.server.groups.contains(g) {
                    existing.server.groups.push(g.clone());
                }
            }
            continue;
        }
        let mut k = k;
        if out.iter().any(|e| e.server.name == k.server.name) {
            k.server.name = format!("{}-{}", k.server.name, k.server.groups.first().cloned().unwrap_or_default());
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
pub fn resolve(known: &[Known], spec: &str) -> Option<Server> {
    if let Some(k) = known.iter().find(|k| k.server.name == spec) {
        return Some(k.server.clone());
    }
    let (user, host, port) = parse_destination(spec)?;
    let probe = Server { host, user, port, ..Default::default() };
    if let Some(k) =
        known.iter().find(|k| k.server.same_target(&probe) && probe.user.is_some() == k.server.user.is_some())
    {
        return Some(k.server.clone());
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
        let opts = s.ssh_options(&|j| (j == "bastion").then(|| "admin@b.lan:22".to_string()));
        assert_eq!(opts, argv("-p 2222 -J admin@b.lan:22"));
        assert_eq!(s.address(), "h:2222");
    }
}
