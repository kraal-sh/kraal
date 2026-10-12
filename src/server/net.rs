//! Network helpers in the server: SSH tunnels kraal keeps open (reconnecting them when they
//! drop), and the local ports that programs in panes listen on (dev servers).

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Server;
use crate::layout::PaneId;

/// What a tunnel forwards; kept across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    /// A known server's name, or the address of `via`.
    pub server: String,
    /// The connection itself, for a server kraal does not know by name: resolving a short name
    /// again later could find another machine, and an address alone loses the key and the jump
    /// host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<crate::servers::Server>,
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
    /// The last line ssh wrote on stderr (not its debug lines).
    said: Arc<Mutex<String>>,
    /// ssh said it listens on the local port and made the listener: the tunnel is up (a port
    /// that listens could be another program's).
    ready: Arc<AtomicBool>,
    /// Stops the reader of ssh's stderr, which a helper ssh started may keep open.
    stop_reading: Arc<AtomicBool>,
    started: Instant,
    retry_at: Option<Instant>,
    failures: u32,
}

impl Tunnel {
    /// Stop its ssh and what that started, and the reader of its stderr; how ssh ended, if it
    /// ran.
    fn stop(&mut self) -> Option<std::process::ExitStatus> {
        self.stop_reading.store(true, Ordering::SeqCst);
        let mut c = self.child.take()?;
        crate::exec::kill_group(&mut c);
        c.try_wait().ok().flatten()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelInfo {
    pub id: u32,
    pub spec: TunnelSpec,
    pub state: TunnelState,
    pub error: Option<String>,
}

/// Whether this machine's `port` can be listened on; why not otherwise.
pub fn port_free(port: u16) -> Result<(), String> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            Err(format!("localhost:{port} is used by another program"))
        }
        Err(e) => Err(format!("cannot listen on localhost:{port}: {e}")),
    }
}

/// Whether a tunnel's ssh (`-v`) has its local listener, from what it says. "Local forwarding
/// listening on" comes before ssh binds the port, so it alone proves nothing: the next line says
/// how that went, `bind …: Address already in use` or the listener's new channel (`channel 0:
/// new port-listener [port listener]`, `channel 0: new [port listener]` in older versions), which
/// ssh creates only once the port listens.
struct Listener {
    announced: String,
    heard: bool,
}

impl Listener {
    fn new(port: u16) -> Listener {
        Listener { announced: format!("Local forwarding listening on 127.0.0.1 port {port}."), heard: false }
    }

    /// Read one line of ssh's stderr; `true` when it shows the listener exists.
    fn read(&mut self, line: &str) -> bool {
        if line.ends_with(&self.announced) {
            self.heard = true;
            return false;
        }
        if !self.heard {
            return false;
        }
        if line.contains(": new ") && line.contains("[port listener]") {
            self.heard = false;
            return true;
        }
        if line.starts_with("bind ") || line.contains("cannot listen") {
            self.heard = false;
        }
        false
    }
}

/// The `-L` forwarding of a tunnel: an IPv6 host in brackets.
fn forward_spec(spec: &TunnelSpec) -> String {
    let host =
        if spec.remote_host.contains(':') { format!("[{}]", spec.remote_host) } else { spec.remote_host.clone() };
    format!("127.0.0.1:{}:{host}:{}", spec.local, spec.remote_port)
}

/// A port nobody listens on, preferring `wanted`, and none of `taken` (ports other tunnels have
/// that may not listen yet: their ssh is still connecting).
pub fn free_port(wanted: u16, taken: &[u16]) -> Option<u16> {
    free_port_with(wanted, taken, |p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok())
}

fn free_port_with(wanted: u16, taken: &[u16], listenable: impl Fn(u16) -> bool) -> Option<u16> {
    let free = |p: u16| !taken.contains(&p) && listenable(p);
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

pub use crate::process::listening_ports;

/// Hosts that name this machine, as they come before `:port`.
const LOCAL_HOSTS: [&[u8]; 5] = [b"localhost", b"127.0.0.1", b"0.0.0.0", b"[::1]", b"[::]"];
/// The longest of them.
const LONGEST_HOST: usize = 9;
/// Digits read after the colon (more are not looked at).
const PORT_DIGITS: u8 = 5;

/// Finds the local ports a pane's output mentions (`localhost:5173`, `127.0.0.1:8000`,
/// `0.0.0.0:3000`, `[::1]:8080`, `http://localhost:4000/`) as it is read, in one pass over each
/// read: a mention cut between two reads is found too.
#[derive(Debug, Default)]
pub struct PortScanner {
    /// The last bytes of the output read so far, for a host the read cut.
    tail: [u8; LONGEST_HOST],
    tail_len: usize,
    /// A mention whose digits ran to the end of the last read: the port so far and its digits.
    digits: Option<(u32, u8)>,
}

impl PortScanner {
    /// Read `text`, the output that follows what was read before, calling `found` with the
    /// ports it mentions (1024 and up). A port cut by the end of `text` is given as far as it
    /// goes, then again whole if more digits follow.
    pub fn scan(&mut self, text: &[u8], mut found: impl FnMut(u16)) {
        let mut report = |port: u32, n: u8| {
            if n > 0
                && let Ok(p) = u16::try_from(port)
                && p >= 1024
            {
                found(p);
            }
        };
        if let Some((port, n)) = self.digits.take() {
            let (port, n, used) = read_digits(text, port, n);
            if used == text.len() && n < PORT_DIGITS {
                self.digits = Some((port, n));
            }
            // Without new digits, it was given already.
            if used > 0 {
                report(port, n);
            }
        }
        for colon in memchr::memchr_iter(b':', text) {
            if !self.host_before(text, colon) {
                continue;
            }
            let rest = &text[colon + 1..];
            let (port, n, used) = read_digits(rest, 0, 0);
            if used == rest.len() && n < PORT_DIGITS {
                self.digits = Some((port, n));
            }
            report(port, n);
        }
        self.keep_tail(text);
    }

    /// Whether a local host ends just before `text[colon]`, counting what was read before.
    fn host_before(&self, text: &[u8], colon: usize) -> bool {
        // The byte before the colon rules out almost everything at once.
        let last = if colon > 0 { Some(text[colon - 1]) } else { self.tail[..self.tail_len].last().copied() };
        if !matches!(last, Some(b't' | b'1' | b'0' | b']')) {
            return false;
        }
        if colon >= LONGEST_HOST {
            let before = &text[colon - LONGEST_HOST..colon];
            return LOCAL_HOSTS.iter().any(|h| before.ends_with(h));
        }
        let mut joined = [0u8; 2 * LONGEST_HOST];
        let (old, new) = (&self.tail[..self.tail_len], &text[..colon]);
        joined[..old.len()].copy_from_slice(old);
        joined[old.len()..old.len() + new.len()].copy_from_slice(new);
        let before = &joined[..old.len() + new.len()];
        LOCAL_HOSTS.iter().any(|h| before.ends_with(h))
    }

    fn keep_tail(&mut self, text: &[u8]) {
        if text.len() >= LONGEST_HOST {
            self.tail.copy_from_slice(&text[text.len() - LONGEST_HOST..]);
            self.tail_len = LONGEST_HOST;
            return;
        }
        let keep = (LONGEST_HOST - text.len()).min(self.tail_len);
        self.tail.copy_within(self.tail_len - keep..self.tail_len, 0);
        self.tail[keep..keep + text.len()].copy_from_slice(text);
        self.tail_len = keep + text.len();
    }
}

/// The digits at the start of `text` that continue a port of `n` digits so far, up to
/// [`PORT_DIGITS`]: the port, its digits, and how many bytes of `text` they took.
fn read_digits(text: &[u8], mut port: u32, mut n: u8) -> (u32, u8, usize) {
    let mut used = 0;
    while n < PORT_DIGITS
        && let Some(d) = text.get(used).filter(|b| b.is_ascii_digit())
    {
        port = port * 10 + u32::from(d - b'0');
        n += 1;
        used += 1;
    }
    (port, n, used)
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
        let s = self.resolve_server(server).with_context(|| format!("not a server: {server}"))?;
        self.add_tunnel_to(s, remote, local)
    }

    /// The server a tunnel goes through.
    pub fn tunnel_target(&self, spec: &TunnelSpec) -> Option<crate::servers::Server> {
        spec.via.clone().or_else(|| self.resolve_server(&spec.server))
    }

    /// Open (and keep open) a tunnel to `remote` through the server `s` (a known one, or a
    /// connection as a pane made it).
    pub fn add_tunnel_to(&mut self, s: crate::servers::Server, remote: &str, local: Option<u16>) -> Result<TunnelInfo> {
        let (remote_host, remote_port) = parse_remote(remote)?;
        let taken: Vec<u16> = self.tunnels.iter().map(|t| t.spec.local).collect();
        if let Some(p) = local.filter(|p| taken.contains(p)) {
            bail!("localhost:{p} is already forwarded");
        }
        let local = match local {
            // Another program on that port would answer in the tunnel's place.
            Some(p) => {
                port_free(p).map_err(anyhow::Error::msg)?;
                p
            }
            None => free_port(remote_port, &taken).context("no free local port")?,
        };
        let named = self.known_servers.iter().any(|k| k.server.name == s.name && k.server.same_connection(&s));
        let spec = if named {
            TunnelSpec { server: s.name.clone(), via: None, local, remote_host, remote_port }
        } else {
            TunnelSpec { server: s.address(), via: Some(s), local, remote_host, remote_port }
        };
        let id = self.alloc_id();
        let mut t = Tunnel {
            id,
            spec,
            state: TunnelState::Connecting,
            error: None,
            child: None,
            said: Default::default(),
            ready: Default::default(),
            stop_reading: Default::default(),
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

    /// Close the tunnel on local port `key` (`5432` or `:5432`), or with id `#7`.
    pub fn remove_tunnel(&mut self, key: &str) -> Result<()> {
        let i = match key.strip_prefix('#') {
            Some(id) => {
                let id: u32 = id.parse().with_context(|| format!("not a tunnel id: {key}"))?;
                self.tunnels.iter().position(|t| t.id == id)
            }
            None => {
                let port: u16 = key.trim_start_matches(':').parse().with_context(|| format!("not a port: {key}"))?;
                self.tunnels.iter().position(|t| t.spec.local == port)
            }
        }
        .with_context(|| format!("no tunnel {key}"))?;
        let mut t = self.tunnels.remove(i);
        t.stop();
        self.mark_persist();
        self.mark_dirty();
        Ok(())
    }

    fn start_tunnel(&self, t: &mut Tunnel) {
        let Some(s) = self.tunnel_target(&t.spec) else {
            t.state = TunnelState::Down;
            t.error = Some(format!("unknown server {}", t.spec.server));
            return;
        };
        let known = &self.known_servers;
        // `-v`: ssh then says when it listens on the local port, which is how the tunnel shows
        // it is up.
        let mut args: Vec<String> = [
            "-N",
            "-v",
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
        args.extend(crate::servers::ssh_args(&s, known, false));
        args.push("-L".into());
        args.push(forward_spec(&t.spec));
        args.push(s.destination());
        // The port listening is how the tunnel shows it is up: it must not be another program's.
        if let Err(why) = port_free(t.spec.local) {
            t.state = TunnelState::Down;
            t.error = Some(why);
            t.failures += 1;
            t.retry_at = Some(Instant::now() + Duration::from_secs((2u64 << t.failures.min(5)).min(60)));
            return;
        }
        let mut cmd = Command::new("ssh");
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        // In a group of its own: a ProxyCommand helper goes with it.
        match crate::exec::own_group(&mut cmd).spawn() {
            Ok(mut c) => {
                t.said = Default::default();
                t.ready = Default::default();
                t.stop_reading = Default::default();
                if let Some(err) = c.stderr.take() {
                    let (said, ready) = (t.said.clone(), t.ready.clone());
                    let mut listener = Listener::new(t.spec.local);
                    crate::exec::read_lines_until(err, t.stop_reading.clone(), move |line| {
                        if listener.read(line) {
                            ready.store(true, Ordering::SeqCst);
                        } else if !line.is_empty()
                            && !line.starts_with("debug")
                            && let Ok(mut s) = said.lock()
                        {
                            *s = line.chars().take(300).collect();
                        }
                    });
                }
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
        let mut tunnels = std::mem::take(&mut self.tunnels);
        let mut changed = false;
        let mut events = Vec::new();
        for t in &mut tunnels {
            if t.child.as_ref().is_some_and(crate::exec::has_exited) {
                // What ssh started (a ProxyCommand) goes too, before ssh is reaped: until then
                // its group id cannot be another's.
                let status = t.stop();
                t.failures += 1;
                t.state = TunnelState::Down;
                let msg = t.said.lock().map(|s| s.clone()).unwrap_or_default();
                let status = status.map(|s| s.to_string()).unwrap_or_else(|| "ended".into());
                t.error = Some(if msg.is_empty() { format!("ssh exited ({status})") } else { msg });
                events.push(("tunnel.down", t.spec.clone(), t.error.clone()));
                let backoff = Duration::from_secs((2u64 << t.failures.min(5)).min(60));
                t.retry_at = Some(Instant::now() + backoff);
                changed = true;
            }
            if t.child.is_none() && t.retry_at.is_some_and(|r| r <= Instant::now()) {
                t.retry_at = None;
                self.start_tunnel(t);
                changed = true;
            }
            if t.state == TunnelState::Connecting && t.child.is_some() && t.ready.load(Ordering::SeqCst) {
                t.state = TunnelState::Up;
                t.error = None;
                t.failures = 0;
                changed = true;
                events.push(("tunnel.up", t.spec.clone(), None));
            }
        }
        self.tunnels = tunnels;
        if changed {
            self.mark_dirty();
        }
        for (ty, spec, error) in events {
            let fields = serde_json::json!({
                "server": spec.server,
                "local": spec.local,
                "remote": format!("{}:{}", spec.remote_host, spec.remote_port),
                "error": error,
            });
            self.emit(ty, fields);
        }
        self.open_pending();
    }

    /// Open the links waiting for their tunnel: when it is up; never when it failed.
    pub(super) fn open_pending(&mut self) {
        let pending = std::mem::take(&mut self.pending_opens);
        for (client, tunnel, url) in pending {
            match self.tunnels.iter().find(|t| t.id == tunnel).map(|t| (t.state, t.error.clone())) {
                Some((TunnelState::Up, _)) => self.open_for(client, &url),
                Some((TunnelState::Down, error)) => {
                    let why = error.unwrap_or_else(|| "the tunnel is down".into());
                    self.toast(client, format!("not opening {url}: {why}"));
                }
                Some((TunnelState::Connecting, _)) => self.pending_opens.push((client, tunnel, url)),
                None => {}
            }
        }
    }

    pub(super) fn stop_tunnels(&mut self) {
        for t in &mut self.tunnels {
            t.stop();
        }
    }

    /// Remember ports a pane's output mentions (dev servers); kept while something listens.
    pub(super) fn note_ports(&mut self, pane: PaneId, data: &[u8]) {
        let Some(p) = self.panes.get_mut(&pane).filter(|p| p.ssh.is_none()) else { return };
        let candidates = &mut self.port_candidates;
        p.ports.scan(data, |port| {
            candidates.insert(port, pane);
        });
    }

    /// Keep only the mentioned ports that something listens on.
    pub(super) fn refresh_ports(&mut self) {
        // No port mentioned, none listed: nothing to look up.
        if self.port_candidates.is_empty() && self.ports.is_empty() {
            return;
        }
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
            let (opened, closed) = super::events::port_changes(&self.ports, &ports);
            self.ports = ports;
            self.mark_dirty();
            for (port, pane) in opened {
                self.emit_pane(
                    "port.opened",
                    pane,
                    serde_json::json!({ "port": port, "url": format!("http://localhost:{port}") }),
                );
            }
            for port in closed {
                self.emit("port.closed", serde_json::json!({ "port": port }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ports `text` mentions, read at once.
    fn ports_in_text(text: &[u8]) -> Vec<u16> {
        let mut out = Vec::new();
        PortScanner::default().scan(text, |p| {
            if !out.contains(&p) {
                out.push(p);
            }
        });
        out
    }

    #[test]
    fn ports_in_output() {
        let text = b"  VITE ready\n  -> Local:   http://localhost:5173/\n  Network: http://0.0.0.0:5173\nlistening on 127.0.0.1:8000, also [::1]:9000 and localhost:22";
        assert_eq!(ports_in_text(text), vec![5173, 8000, 9000]);
        assert!(ports_in_text(b"no ports here: localhost:abc").is_empty());
        // Five digits at most are read, and a port is at most 65535.
        assert_eq!(ports_in_text(b"localhost:123456 localhost:99999 [::]:08080"), vec![12345, 8080]);
        assert!(ports_in_text(b"localhost:\n:5173 localhost5173 127.0.0.2:8000").is_empty());
    }

    /// The ports found when the output comes in reads cut at `cuts`.
    fn ports_read_in(text: &[u8], cuts: &[usize]) -> Vec<u16> {
        let mut scanner = PortScanner::default();
        let mut found = Vec::new();
        let mut start = 0;
        for &end in cuts.iter().chain([&text.len()]) {
            scanner.scan(&text[start..end], |p| {
                if !found.contains(&p) {
                    found.push(p);
                }
            });
            start = end;
        }
        found
    }

    #[test]
    fn a_port_cut_between_reads_is_found() {
        let text = b"Local: http://localhost:5173/ and 127.0.0.1:8000\n[::1]:9000 at last localhost:4000";
        let whole = ports_in_text(text);
        assert_eq!(whole, vec![5173, 8000, 9000, 4000]);
        // Every cut, and reads of one byte each.
        for cut in 0..=text.len() {
            assert_eq!(ports_read_in(text, &[cut]), whole, "cut at {cut}");
        }
        assert_eq!(ports_read_in(text, &(1..text.len()).collect::<Vec<_>>()), whole);
        // Digits a read cuts are completed by the next one.
        assert_eq!(ports_read_in(b"localhost:8080 localhost:3000", &[11, 12]), vec![8080, 3000]);
        assert_eq!(ports_read_in(b"[::]:1234567", &[6, 7, 9]), vec![1234, 12345]);
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
        // A port something listens on is not given (held for the whole test: no other test can
        // take it meanwhile).
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        assert_ne!(free_port(port, &[]), Some(port));
        // The wanted port when it is free, and not when another tunnel has it (its ssh still
        // connects, so nothing listens there yet); whether a port can be listened on is the
        // test's to say, not the system's, so no other test can change the answer.
        assert_eq!(free_port_with(5000, &[], |_| true), Some(5000));
        assert_eq!(free_port_with(5000, &[5000], |_| true), Some(10000));
        assert_eq!(free_port_with(5000, &[10000], |p| p != 5000), Some(10001));
        assert_eq!(free_port_with(80, &[], |_| true), Some(10000), "a privileged port is never asked for");
        drop(busy);
    }

    #[test]
    fn a_tunnel_is_up_once_ssh_has_its_listener() {
        // As OpenSSH 10.6 says it with -v, then as older versions name the channel.
        let mut l = Listener::new(40279);
        assert!(!l.read("debug1: Authenticated to 127.0.0.1 ([127.0.0.1]:22999) using \"publickey\"."));
        assert!(!l.read("debug1: Local forwarding listening on 127.0.0.1 port 40279."));
        assert!(l.read("debug1: channel 0: new port-listener [port listener] (inactive timeout: 0.000000)"));
        let mut old = Listener::new(5432);
        assert!(!old.read("debug1: Local forwarding listening on 127.0.0.1 port 5432."));
        assert!(old.read("debug1: channel 0: new [port listener]"));
        // The port taken by another program: announced, then not made.
        let mut busy = Listener::new(40555);
        assert!(!busy.read("debug1: Local forwarding listening on 127.0.0.1 port 40555."));
        assert!(!busy.read("bind [127.0.0.1]:40555: Address already in use"));
        assert!(!busy.read("channel_setup_fwd_listener_tcpip: cannot listen to port: 40555"));
        assert!(!busy.read("debug1: channel 1: new [port listener]"), "a listener for something else");
        // Another port's announcement is not this one's.
        let mut other = Listener::new(1);
        assert!(!other.read("debug1: Local forwarding listening on 127.0.0.1 port 11."));
        assert!(!other.read("debug1: channel 0: new [port listener]"));
    }

    #[test]
    fn forwarding_specs() {
        let t =
            TunnelSpec { server: "db".into(), via: None, local: 15432, remote_host: "::1".into(), remote_port: 5432 };
        assert_eq!(forward_spec(&t), "127.0.0.1:15432:[::1]:5432");
        let t = TunnelSpec { remote_host: "db.internal".into(), ..t };
        assert_eq!(forward_spec(&t), "127.0.0.1:15432:db.internal:5432");
        // A port held here is refused. (That it is free again once dropped cannot be checked:
        // a test running alongside may take it in between.)
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        assert!(port_free(port).unwrap_err().contains("another program"));
    }

    #[test]
    fn descriptions() {
        let t = TunnelSpec {
            server: "db".into(),
            via: None,
            local: 15432,
            remote_host: "localhost".into(),
            remote_port: 5432,
        };
        assert_eq!(t.describe(), "db:5432 → localhost:15432");
    }
}
