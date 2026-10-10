//! File transfers between this machine and servers, or between two servers: `kraal cp`.
//!
//! Endpoints are local paths or `server:path`, where `server` is an inventory name or
//! `user@host[:port]`. rsync is used when available (resumable, with progress), scp otherwise;
//! server-to-server copies stream a tar archive through this machine so each side keeps its own
//! port, jump host and key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::servers::{self, Known, Server};

#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    Local(PathBuf),
    Remote { server: Server, path: String },
}

impl Endpoint {
    pub fn display(&self) -> String {
        match self {
            Endpoint::Local(p) => p.display().to_string(),
            Endpoint::Remote { server, path } => format!("{}:{path}", server.name),
        }
    }
}

/// Parse `path`, `server:path` or `user@host:path`.
pub fn parse_endpoint(s: &str, known: &[Known]) -> Result<Endpoint> {
    if let Some((head, path)) = split_remote(s) {
        if let Some(server) = servers::resolve(known, head)
            && (known.iter().any(|k| k.server.name == head) || head.contains('@') || head.contains('.'))
        {
            return Ok(Endpoint::Remote { server, path: path.to_string() });
        }
        if !Path::new(s).exists() {
            bail!("unknown server {head:?} (see `kraal servers`)");
        }
    }
    Ok(Endpoint::Local(PathBuf::from(s)))
}

/// `head:path` when the text before the first `:` looks like a host, not a path.
pub(crate) fn split_remote(s: &str) -> Option<(&str, &str)> {
    if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with('~') {
        return None;
    }
    // `[v6]:path` and `user@[v6]:path`
    if let Some(close) = s.find("]:") {
        return Some((&s[..close + 1], &s[close + 2..]));
    }
    let (head, path) = s.split_once(':')?;
    (!head.is_empty() && !head.contains('/')).then_some((head, path))
}

/// Remote paths: `~/x` and `~` become home-relative, which rsync and scp understand.
fn remote_path(p: &str) -> String {
    if p == "~" {
        return String::new();
    }
    p.strip_prefix("~/").unwrap_or(p).to_string()
}

/// Quote for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One step of a transfer.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub program: String,
    pub args: Vec<String>,
    /// Command to run instead if this one fails because rsync is missing on the server.
    pub fallback: Option<Box<Step>>,
}

impl Step {
    fn new(program: &str, args: Vec<String>) -> Step {
        Step { program: program.to_string(), args, fallback: None }
    }

    pub fn shell_line(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(|s| s.as_str()))
            .map(sh_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// How to reach a server: ssh options (without destination) for each server.
pub type SshOptions<'a> = &'a dyn Fn(&Server) -> Vec<String>;

/// scp spells the port option `-P`.
fn scp_options(ssh: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ssh.len() {
        if ssh[i] == "-p" && i + 1 < ssh.len() {
            out.extend(["-P".to_string(), ssh[i + 1].clone()]);
            i += 2;
            continue;
        }
        out.push(ssh[i].clone());
        i += 1;
    }
    out
}

/// Build the commands that copy `sources` to `dest`.
pub fn plan(sources: &[Endpoint], dest: &Endpoint, ssh: SshOptions, have_rsync: bool) -> Result<Vec<Step>> {
    if sources.is_empty() {
        bail!("nothing to copy");
    }
    let remote_servers: Vec<&Server> = sources
        .iter()
        .filter_map(|s| match s {
            Endpoint::Remote { server, .. } => Some(server),
            Endpoint::Local(_) => None,
        })
        .collect();
    let all_local = remote_servers.is_empty();
    let all_remote = remote_servers.len() == sources.len();
    if !all_local && !all_remote {
        bail!("copy local files and remote files in separate commands");
    }
    if all_remote && remote_servers.iter().any(|s| s.destination() != remote_servers[0].destination()) {
        bail!("all remote sources must be on the same server");
    }
    for s in sources {
        if let Endpoint::Local(p) = s
            && !p.exists()
        {
            bail!("{} does not exist", p.display());
        }
    }
    let local_args = |eps: &[Endpoint]| -> Vec<String> {
        eps.iter()
            .filter_map(|e| match e {
                Endpoint::Local(p) => Some(p.display().to_string()),
                _ => None,
            })
            .collect()
    };
    let remote_paths = |eps: &[Endpoint]| -> Vec<String> {
        eps.iter()
            .filter_map(|e| match e {
                Endpoint::Remote { path, .. } => Some(remote_path(path)),
                _ => None,
            })
            .collect()
    };

    match (all_local, dest) {
        (true, Endpoint::Local(d)) => {
            let mut args = vec!["-a".to_string()];
            args.extend(local_args(sources));
            args.push(d.display().to_string());
            if have_rsync {
                let mut rs = vec!["-a".to_string(), "--info=progress2".to_string()];
                rs.extend(local_args(sources));
                rs.push(d.display().to_string());
                Ok(vec![Step::new("rsync", rs)])
            } else {
                args[0] = "-R".into();
                Ok(vec![Step::new("cp", args)])
            }
        }
        (true, Endpoint::Remote { server, path }) => {
            let opts = ssh(server);
            let target = format!("{}:{}", server.destination(), remote_path(path));
            let mut scp = vec!["-r".to_string()];
            scp.extend(scp_options(&opts));
            scp.extend(local_args(sources));
            scp.push(target.clone());
            let scp = Step::new("scp", scp);
            if !have_rsync {
                return Ok(vec![scp]);
            }
            let mut rs = vec![
                "-a".to_string(),
                "--partial".to_string(),
                "--info=progress2".to_string(),
                "-e".to_string(),
                ssh_e(&opts),
            ];
            rs.extend(local_args(sources));
            rs.push(target);
            Ok(vec![Step { fallback: Some(Box::new(scp)), ..Step::new("rsync", rs) }])
        }
        (false, Endpoint::Local(d)) => {
            let server = remote_servers[0];
            let opts = ssh(server);
            let srcs: Vec<String> =
                remote_paths(sources).into_iter().map(|p| format!("{}:{p}", server.destination())).collect();
            let mut scp = vec!["-r".to_string()];
            scp.extend(scp_options(&opts));
            scp.extend(srcs.iter().cloned());
            scp.push(d.display().to_string());
            let scp = Step::new("scp", scp);
            if !have_rsync {
                return Ok(vec![scp]);
            }
            let mut rs = vec![
                "-a".to_string(),
                "--partial".to_string(),
                "--info=progress2".to_string(),
                "-e".to_string(),
                ssh_e(&opts),
            ];
            rs.extend(srcs);
            rs.push(d.display().to_string());
            Ok(vec![Step { fallback: Some(Box::new(scp)), ..Step::new("rsync", rs) }])
        }
        (false, Endpoint::Remote { server: to, path: to_path }) => {
            let from = remote_servers[0];
            let paths = remote_paths(sources);
            if from.destination() == to.destination() && from.port == to.port {
                // Same server: copy there.
                let mut remote = String::from("cp -R --");
                for p in &paths {
                    remote.push(' ');
                    remote.push_str(&remote_shell_path(p));
                }
                remote.push(' ');
                remote.push_str(&remote_shell_path(&remote_path(to_path)));
                let mut args = ssh(from);
                args.push(from.destination());
                args.push(remote);
                return Ok(vec![Step::new("ssh", args)]);
            }
            // Two servers: tar on one side, untar on the other, streamed through here.
            let mut pack = String::from("tar cf -");
            for p in &paths {
                let (dir, name) = split_parent(p);
                pack.push_str(&format!(" -C {} {}", remote_shell_path(&dir), sh_quote(&name)));
            }
            let dest_dir = remote_shell_path(&remote_path(to_path));
            let unpack = format!("mkdir -p {dest_dir} && tar xf - -C {dest_dir}");
            let mut left = vec!["ssh".to_string()];
            left.extend(ssh(from));
            left.push(from.destination());
            left.push(pack);
            let mut right = vec!["ssh".to_string()];
            right.extend(ssh(to));
            right.push(to.destination());
            right.push(unpack);
            let pipe = format!(
                "{} | {}",
                left.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" "),
                right.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")
            );
            Ok(vec![Step::new("sh", vec!["-c".to_string(), format!("set -o pipefail 2>/dev/null; {pipe}")])])
        }
    }
}

/// `-e` value for rsync: the ssh command with its options.
fn ssh_e(opts: &[String]) -> String {
    std::iter::once("ssh".to_string()).chain(opts.iter().map(|o| sh_quote(o))).collect::<Vec<_>>().join(" ")
}

/// A remote path for the remote shell: relative paths stay relative to home.
fn remote_shell_path(p: &str) -> String {
    if p.is_empty() { ".".to_string() } else { sh_quote(p) }
}

fn split_parent(p: &str) -> (String, String) {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", name)) => ("/".to_string(), name.to_string()),
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), trimmed.to_string()),
    }
}

/// Delete local paths and remote paths (one `rm -rf` per server).
pub fn remove(targets: &[Endpoint], ssh: SshOptions) -> Result<()> {
    let mut remote: Vec<(Server, Vec<String>)> = Vec::new();
    for t in targets {
        match t {
            Endpoint::Local(p) => {
                let meta = std::fs::symlink_metadata(p).with_context(|| format!("{}", p.display()))?;
                if meta.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
                    .with_context(|| format!("deleting {}", p.display()))?;
            }
            Endpoint::Remote { server, path } => {
                let path = remote_path(path);
                if path.is_empty() || path == "/" || path == "." {
                    bail!("refusing to delete {}:{path}", server.name);
                }
                match remote.iter_mut().find(|(s, _)| s.destination() == server.destination()) {
                    Some((_, v)) => v.push(path),
                    None => remote.push((server.clone(), vec![path])),
                }
            }
        }
    }
    for (server, paths) in remote {
        let mut line = String::from("rm -rf --");
        for p in &paths {
            line.push(' ');
            line.push_str(&sh_quote(p));
        }
        let mut args = ssh(&server);
        args.push(server.destination());
        args.push(line);
        let status = std::process::Command::new("ssh").args(&args).status().context("running ssh")?;
        if !status.success() {
            bail!("deleting on {} failed ({status})", server.name);
        }
    }
    Ok(())
}

pub fn have_program(name: &str) -> bool {
    std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file())).unwrap_or(false)
}

/// Run the steps in the foreground, showing their output. Falls back to scp when rsync is
/// missing on the server (rsync exits with 12 or 127 then).
pub fn run(steps: &[Step]) -> Result<()> {
    for step in steps {
        eprintln!("$ {}", step.shell_line());
        let status = std::process::Command::new(&step.program)
            .args(&step.args)
            .status()
            .with_context(|| format!("running {}", step.program))?;
        if status.success() {
            continue;
        }
        if let Some(fb) = &step.fallback
            && matches!(status.code(), Some(12) | Some(127))
        {
            eprintln!("rsync is not available on the server; using scp");
            eprintln!("$ {}", fb.shell_line());
            let status = std::process::Command::new(&fb.program).args(&fb.args).status()?;
            if status.success() {
                continue;
            }
            bail!("{} failed ({status})", fb.program);
        }
        bail!("{} failed ({status})", step.program);
    }
    Ok(())
}

/// Split text pasted by a terminal when files are dropped on it into paths: whitespace-separated
/// words with shell quoting and backslash escapes. `None` unless every word is an existing
/// absolute local path.
pub fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.trim().chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(cur);
    }
    if words.is_empty() {
        return None;
    }
    let paths: Vec<PathBuf> =
        words.into_iter().map(|w| PathBuf::from(w.strip_prefix("file://").map(String::from).unwrap_or(w))).collect();
    paths.iter().all(|p| p.is_absolute() && p.exists()).then_some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<Known> {
        let mut inv = crate::servers::Inventory::empty();
        inv.servers = vec![
            Server { name: "a".into(), host: "a.lan".into(), user: Some("u".into()), ..Default::default() },
            Server {
                name: "b".into(),
                host: "b.lan".into(),
                user: Some("v".into()),
                port: Some(2222),
                ..Default::default()
            },
        ];
        crate::servers::known(&inv, &[], &[])
    }

    fn opts(s: &Server) -> Vec<String> {
        s.port.map(|p| vec!["-p".to_string(), p.to_string()]).unwrap_or_default()
    }

    #[test]
    fn endpoints() {
        let k = known();
        assert!(
            matches!(parse_endpoint("a:/var/log", &k).unwrap(), Endpoint::Remote { ref path, .. } if path == "/var/log")
        );
        assert!(matches!(parse_endpoint("/tmp/x", &k).unwrap(), Endpoint::Local(_)));
        assert!(matches!(parse_endpoint("./a:b", &k).unwrap(), Endpoint::Local(_)));
        let e = parse_endpoint("me@new.host:~/f", &k).unwrap();
        assert!(matches!(e, Endpoint::Remote { ref server, ref path } if server.host == "new.host" && path == "~/f"));
        assert!(parse_endpoint("nosuch:/x", &k).is_err());
    }

    #[test]
    fn upload_uses_rsync_with_scp_fallback() {
        let k = known();
        let src = Endpoint::Local(std::env::temp_dir());
        let dest = parse_endpoint("b:~/up/", &k).unwrap();
        let steps = plan(&[src], &dest, &opts, true).unwrap();
        assert_eq!(steps[0].program, "rsync");
        assert!(steps[0].args.contains(&"ssh -p 2222".to_string()), "{:?}", steps[0].args);
        assert_eq!(steps[0].args.last().unwrap(), "v@b.lan:up/");
        let fb = steps[0].fallback.as_ref().unwrap();
        assert_eq!(fb.program, "scp");
        assert!(fb.args.windows(2).any(|w| w == ["-P", "2222"]));
    }

    #[test]
    fn download_and_server_to_server() {
        let k = known();
        let src = parse_endpoint("a:/etc/hosts", &k).unwrap();
        let steps = plan(std::slice::from_ref(&src), &Endpoint::Local("/tmp".into()), &opts, false).unwrap();
        assert_eq!(steps[0].program, "scp");
        assert_eq!(steps[0].args, vec!["-r", "u@a.lan:/etc/hosts", "/tmp"]);
        let dest = parse_endpoint("b:/srv/in", &k).unwrap();
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, true).unwrap();
        let line = &steps[0].args[1];
        assert!(line.contains("tar cf - -C /etc hosts"), "{line}");
        assert!(line.contains("-p 2222 v@b.lan"), "{line}");
        let same = parse_endpoint("a:/tmp/", &k).unwrap();
        let steps = plan(&[src], &same, &opts, true).unwrap();
        assert_eq!(steps[0].program, "ssh");
        assert_eq!(steps[0].args.last().unwrap(), "cp -R -- /etc/hosts /tmp/");
    }

    #[test]
    fn mixed_sources_are_rejected() {
        let k = known();
        let a = parse_endpoint("a:/x", &k).unwrap();
        let b = parse_endpoint("b:/y", &k).unwrap();
        assert!(plan(&[a.clone(), b], &Endpoint::Local("/tmp".into()), &opts, true).is_err());
        assert!(
            plan(&[a, Endpoint::Local(std::env::temp_dir())], &Endpoint::Local("/tmp".into()), &opts, true).is_err()
        );
    }

    #[test]
    fn dropped_paths_parsing() {
        let dir = std::env::temp_dir().join(format!("kraal drop {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("it's here.txt");
        std::fs::write(&f, "x").unwrap();
        let escaped = f.display().to_string().replace(' ', "\\ ").replace('\'', "\\'");
        assert_eq!(dropped_paths(&escaped), Some(vec![f.clone()]));
        let quoted = format!("'{}' {}", dir.display(), escaped);
        assert_eq!(dropped_paths(&quoted), Some(vec![dir.clone(), f.clone()]));
        assert_eq!(dropped_paths("echo hello"), None);
        assert_eq!(dropped_paths("relative/path"), None);
        assert_eq!(
            dropped_paths(&format!("file://{}", dir.display().to_string().replace(' ', "\\ "))),
            Some(vec![dir.clone()])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("plain/path"), "plain/path");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(remote_path("~/x"), "x");
        assert_eq!(remote_path("~"), "");
        assert_eq!(split_parent("/etc/hosts"), ("/etc".into(), "hosts".into()));
        assert_eq!(split_parent("dir/"), ("".into(), "dir".into()));
    }
}
