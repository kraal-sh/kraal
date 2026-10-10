//! Network helpers in the server: SSH tunnels kraal keeps open (reconnecting them when they
//! drop), and the local ports that programs in panes listen on (dev servers).

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Server;
use crate::layout::PaneId;

/// What a tunnel forwards; kept across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    pub server: String,
    /// Port on this machine (127.0.0.1).
    pub local: u16,
    /// Host as seen from the server (`localhost` for the server itself).
    pub remote_host: String,
    pub remote_port: u16,
}

impl TunnelSpec {
    pub fn describe(&self) -> String {
        let host = if self.remote_host == "localhost" { String::new() } else { format!("{}:", self.remote_host) };
        format!("{}:{}{} → localhost:{}", self.server, host, self.remote_port, self.local)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelState {
    Up,
    Connecting,
    Down,
}

pub struct Tunnel {
    pub id: u32,
    pub spec: TunnelSpec,
    pub state: TunnelState,
    pub error: Option<String>,
    child: Option<Child>,
    started: Instant,
    retry_at: Option<Instant>,
    failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelInfo {
    pub id: u32,
    pub spec: TunnelSpec,
    pub state: TunnelState,
    pub error: Option<String>,
}

/// A port nobody listens on, preferring `wanted`.
pub fn free_port(wanted: u16) -> Option<u16> {
    let free = |p: u16| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok();
    if wanted >= 1024 && free(wanted) {
        return Some(wanted);
    }
    (10000..60000).find(|p| free(*p))
}

/// Parse `[host:]port`.
pub fn parse_remote(s: &str) -> Result<(String, u16)> {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']).to_string(), p),
        None => ("localhost".to_string(), s),
    };
    let port: u16 = port.parse().with_context(|| format!("not a port: {port}"))?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        bail!("not a host: {host:?}");
    }
    Ok((host, port))
}

/// Ports in LISTEN state on this machine (Linux: /proc/net/tcp and tcp6).
pub fn listening_ports() -> HashSet<u16> {
    let mut out = HashSet::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // local_address is "ADDR:PORT" in hex; state 0A is LISTEN.
            if cols.len() > 3
                && cols[3] == "0A"
                && let Some((_, port)) = cols[1].rsplit_once(':')
                && let Ok(p) = u16::from_str_radix(port, 16)
            {
                out.insert(p);
            }
        }
    }
    out
}

/// Local ports mentioned in program output: `localhost:5173`, `127.0.0.1:8000`, `0.0.0.0:3000`,
/// `[::1]:8080`, `http://localhost:4000/`.
pub fn ports_in_text(text: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    for needle in [&b"localhost:"[..], b"127.0.0.1:", b"0.0.0.0:", b"[::1]:", b"[::]:"] {
        let mut start = 0;
        while let Some(i) = find(&text[start..], needle) {
            let at = start + i + needle.len();
            let digits: Vec<u8> = text[at..].iter().take(5).take_while(|b| b.is_ascii_digit()).copied().collect();
            if let Ok(p) = std::str::from_utf8(&digits).unwrap_or("").parse::<u16>()
                && p >= 1024
                && !out.contains(&p)
            {
                out.push(p);
            }
            start = at;
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Server {
    pub fn tunnel_infos(&self) -> Vec<TunnelInfo> {
        self.tunnels
            .iter()
            .map(|t| TunnelInfo { id: t.id, spec: t.spec.clone(), state: t.state, error: t.error.clone() })
            .collect()
    }

    /// Open (and keep open) a tunnel to `remote` (`[host:]port`) through `server`.
    pub fn add_tunnel(&mut self, server: &str, remote: &str, local: Option<u16>) -> Result<TunnelInfo> {
        let (remote_host, remote_port) = parse_remote(remote)?;
        let s = self.resolve_server(server).with_context(|| format!("not a server: {server}"))?;
        let local = match local {
            Some(p) => p,
            None => free_port(remote_port).context("no free local port")?,
        };
        if self.tunnels.iter().any(|t| t.spec.local == local) {
            bail!("localhost:{local} is already forwarded");
        }
        let spec = TunnelSpec { server: s.name.clone(), local, remote_host, remote_port };
        let id = self.alloc_id();
        let mut t = Tunnel {
            id,
            spec,
            state: TunnelState::Connecting,
            error: None,
            child: None,
            started: Instant::now(),
            retry_at: None,
            failures: 0,
        };
        self.start_tunnel(&mut t);
        let info = TunnelInfo { id, spec: t.spec.clone(), state: t.state, error: t.error.clone() };
        self.tunnels.push(t);
        self.mark_persist();
        self.mark_dirty();
        Ok(info)
    }

    pub fn remove_tunnel(&mut self, key: &str) -> Result<()> {
        let n: u32 = key.trim_start_matches(':').parse().with_context(|| format!("not a tunnel: {key}"))?;
        let i = self
            .tunnels
            .iter()
            .position(|t| t.id == n || u32::from(t.spec.local) == n)
            .with_context(|| format!("no tunnel {key}"))?;
        let mut t = self.tunnels.remove(i);
        if let Some(c) = &mut t.child {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.mark_persist();
        self.mark_dirty();
        Ok(())
    }

    fn start_tunnel(&self, t: &mut Tunnel) {
        let Some(s) = self.resolve_server(&t.spec.server) else {
            t.state = TunnelState::Down;
            t.error = Some(format!("unknown server {}", t.spec.server));
            return;
        };
        let known = &self.known_servers;
        let mut args: Vec<String> = [
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
        ]
        .iter()
        .map(|a| a.to_string())
        .collect();
        args.extend(s.ssh_options(&|j| crate::servers::resolve(known, j).map(|js| js.address())));
        args.push("-L".into());
        args.push(format!("127.0.0.1:{}:{}:{}", t.spec.local, t.spec.remote_host, t.spec.remote_port));
        args.push(s.destination());
        match Command::new("ssh").args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()
        {
            Ok(c) => {
                t.child = Some(c);
                t.state = TunnelState::Connecting;
                t.started = Instant::now();
            }
            Err(e) => {
                t.state = TunnelState::Down;
                t.error = Some(format!("cannot run ssh: {e}"));
            }
        }
    }

    /// Watch tunnels: mark them up once the local port listens, restart them when they drop
    /// (with backoff), report errors.
    pub(super) fn check_tunnels(&mut self) {
        if self.tunnels.is_empty() {
            return;
        }
        let listening = listening_ports();
        let mut tunnels = std::mem::take(&mut self.tunnels);
        let mut changed = false;
        for t in &mut tunnels {
            if let Some(c) = &mut t.child
                && let Ok(Some(status)) = c.try_wait()
            {
                let mut err = String::new();
                if let Some(mut e) = c.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                t.child = None;
                t.failures += 1;
                t.state = TunnelState::Down;
                let msg = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                t.error = Some(if msg.is_empty() { format!("ssh exited ({status})") } else { msg });
                let backoff = Duration::from_secs((2u64 << t.failures.min(5)).min(60));
                t.retry_at = Some(Instant::now() + backoff);
                changed = true;
            }
            if t.child.is_none() && t.retry_at.is_some_and(|r| r <= Instant::now()) {
                t.retry_at = None;
                self.start_tunnel(t);
                changed = true;
            }
            if t.state == TunnelState::Connecting && t.child.is_some() && listening.contains(&t.spec.local) {
                t.state = TunnelState::Up;
                t.error = None;
                t.failures = 0;
                changed = true;
            }
        }
        self.tunnels = tunnels;
        if changed {
            self.mark_dirty();
        }
    }

    pub(super) fn stop_tunnels(&mut self) {
        for t in &mut self.tunnels {
            if let Some(c) = &mut t.child {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    /// Remember ports a pane's output mentions (dev servers); kept while something listens.
    pub(super) fn note_ports(&mut self, pane: PaneId, data: &[u8]) {
        if self.panes.get(&pane).is_some_and(|p| p.ssh.is_some()) {
            return;
        }
        for p in ports_in_text(data) {
            self.port_candidates.insert(p, pane);
        }
    }

    /// Keep only the mentioned ports that something listens on.
    pub(super) fn refresh_ports(&mut self) {
        let listening = listening_ports();
        let mut ports: BTreeMap<u16, PaneId> = BTreeMap::new();
        for (port, pane) in &self.port_candidates {
            if listening.contains(port)
                && self.panes.contains_key(pane)
                && !self.tunnels.iter().any(|t| t.spec.local == *port)
            {
                ports.insert(*port, *pane);
            }
        }
        // A port mentioned before its server is up may start listening later; forget it only
        // with its pane, and keep the list short.
        self.port_candidates.retain(|_, pane| self.panes.contains_key(pane));
        if self.port_candidates.len() > 64 {
            let mut keep: Vec<u16> = self.port_candidates.keys().copied().collect();
            keep.sort_unstable();
            keep.truncate(64);
            self.port_candidates.retain(|p, _| keep.contains(p));
        }
        if ports != self.ports {
            self.ports = ports;
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_in_output() {
        let text = b"  VITE ready\n  -> Local:   http://localhost:5173/\n  Network: http://0.0.0.0:5173\nlistening on 127.0.0.1:8000, also [::1]:9000 and localhost:22";
        assert_eq!(ports_in_text(text), vec![5173, 8000, 9000]);
        assert!(ports_in_text(b"no ports here: localhost:abc").is_empty());
    }

    #[test]
    fn remotes() {
        assert_eq!(parse_remote("5432").unwrap(), ("localhost".to_string(), 5432));
        assert_eq!(parse_remote("db.internal:5432").unwrap(), ("db.internal".to_string(), 5432));
        assert_eq!(parse_remote("[::1]:80").unwrap(), ("::1".to_string(), 80));
        assert!(parse_remote("x:notaport").is_err());
    }

    #[test]
    fn free_ports() {
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        let got = free_port(port).unwrap();
        assert_ne!(got, port);
        assert!(listening_ports().contains(&port) || !std::path::Path::new("/proc/net/tcp").exists());
    }

    #[test]
    fn descriptions() {
        let t = TunnelSpec { server: "db".into(), local: 15432, remote_host: "localhost".into(), remote_port: 5432 };
        assert_eq!(t.describe(), "db:5432 → localhost:15432");
    }
}
