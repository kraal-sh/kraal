//! What other processes are doing: command line, environment, working directory, the TCP
//! ports listening on this machine, and who is behind a Unix socket. Linux reads `/proc`; macOS
//! asks the kernel (sysctl and libproc) and `lsof` for ports.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Command line of a process.
pub fn argv(pid: u32) -> Option<Vec<String>> {
    let (argv, _) = imp::args_and_env(pid)?;
    (!argv.is_empty()).then_some(argv)
}

/// Some variables of a process's environment, as it was started.
pub fn environ(pid: u32, names: &[&str]) -> HashMap<String, String> {
    let Some((_, env)) = imp::args_and_env(pid) else { return HashMap::new() };
    env.into_iter()
        .filter_map(|e| {
            let (k, v) = e.split_once('=')?;
            names.contains(&k).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Current directory of a process.
pub fn cwd(pid: u32) -> Option<PathBuf> {
    imp::cwd(pid)
}

/// TCP ports something listens on, on this machine.
pub fn listening_ports() -> HashSet<u16> {
    imp::listening_ports()
}

/// Process id and user of the process that listens on the other end of `stream`.
pub fn socket_peer(stream: &UnixStream) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
    imp::socket_peer(stream.as_raw_fd())
}

/// Whether a process with this id still runs. A process that exited but was not reaped yet
/// counts as gone where the system tells (Linux).
pub fn running(pid: libc::pid_t) -> bool {
    imp::running(pid)
}

/// The processes (id, program name) whose working directory is `dir` or inside it, or that hold
/// a file open there: what may still write there (a shell, an agent, an editor, a
/// build writing through absolute paths). Not this process.
pub fn working_in(dir: &Path) -> Vec<(u32, String)> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let me = std::process::id();
    imp::pids()
        .into_iter()
        .filter(|p| {
            *p != me
                && (cwd(*p).is_some_and(|c| c.starts_with(&dir))
                    || imp::open_files(*p).iter().any(|f| f.starts_with(&dir)))
        })
        .map(|p| {
            let name = argv(p).and_then(|a| a.into_iter().next()).unwrap_or_default();
            (p, name.rsplit('/').next().unwrap_or(&name).to_string())
        })
        .collect()
}

/// The processes started with `name=value` in their environment (inherited from whatever kraal
/// started with it), even those that left their parent, group and session: a daemon started by
/// a wrapper keeps the environment it was started with. Not this process.
pub fn tagged(name: &str, value: &str) -> Vec<u32> {
    let me = std::process::id();
    imp::pids().into_iter().filter(|p| *p != me && environ(*p, &[name]).get(name).is_some_and(|v| v == value)).collect()
}

/// Every process descending from `pid` (children, theirs, and so on), even those that left its
/// process group or session (`setsid`): while `pid` is alive, they are still its descendants.
pub fn descendants(pid: u32) -> Vec<u32> {
    let parents: Vec<(u32, u32)> = imp::pids().into_iter().filter_map(|p| Some((p, imp::ppid(p)?))).collect();
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(parents.iter().filter(|(_, pp)| *pp == parent).map(|(p, _)| *p));
        i += 1;
    }
    found.remove(0);
    found
}

/// Every process running on this machine that the system shows us.
pub fn pids() -> Vec<u32> {
    imp::pids()
}

/// When a process started, as a number that, with its id, names it and no other process of this
/// boot (Linux: clock ticks after boot, see [`boot_id`]; macOS: microseconds since 1970).
pub fn start_time(pid: u32) -> Option<u64> {
    imp::start_time(pid)
}

/// This machine as its system names it (Linux's machine-id, macOS's host UUID), if it does.
pub fn machine_id() -> Option<String> {
    imp::machine_id()
}

/// This boot of the machine where start times count from it (Linux); empty elsewhere.
pub fn boot_id() -> String {
    imp::boot_id()
}

/// Whether something still runs in process group `pgid`. A member that ended but was not reaped
/// yet does not count: a pane keeps its ended leader unreaped (see `pane::pty::Process`).
pub fn group_running(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the group has members, zombies included.
    if pgid <= 1 || unsafe { libc::kill(-pgid, 0) } != 0 {
        return false;
    }
    imp::group_running(pgid)
}

/// The processes of session `sid` (a pane's program leads one, with everything started from
/// it that did not leave), zombies left out.
pub fn session_members(sid: libc::pid_t) -> Vec<libc::pid_t> {
    if sid <= 1 {
        return Vec::new();
    }
    imp::session_members(sid)
}

/// Whether a process with this id exists, zombies included.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Split NUL-separated strings, skipping empty ones (Linux `/proc` files).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn nul_strings(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

/// Parse a `KERN_PROCARGS2` buffer (macOS): argc, the executable path, padding, argv, then
/// the environment.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs2(buf: &[u8]) -> Option<(Vec<String>, Vec<String>)> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    // Skip the executable path and the NULs that pad it.
    let path_end = rest.iter().position(|b| *b == 0)?;
    let start = path_end + rest[path_end..].iter().position(|b| *b != 0)?;
    let strings: Vec<String> =
        rest[start..].split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let argv: Vec<String> = strings.iter().take(argc).cloned().collect();
    let env: Vec<String> = strings.into_iter().skip(argc).take_while(|s| !s.is_empty()).collect();
    Some((argv, env))
}

/// Ports in `lsof -Fn` output (macOS).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_lsof(text: &str) -> HashSet<u16> {
    text.lines()
        .filter_map(|l| l.strip_prefix('n'))
        .filter_map(|addr| addr.rsplit_once(':'))
        .filter_map(|(_, port)| port.parse().ok())
        .collect()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn args_and_env(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
        let argv = nul_strings(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
        let env = std::fs::read(format!("/proc/{pid}/environ")).map(|r| nul_strings(&r)).unwrap_or_default();
        Some((argv, env))
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The files a process holds open (those it may read: ours).
    pub fn open_files(pid: u32) -> Vec<PathBuf> {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
        fds.flatten().filter_map(|fd| std::fs::read_link(fd.path()).ok()).filter(|p| p.is_absolute()).collect()
    }

    pub fn pids() -> Vec<u32> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
    }

    pub fn start_time(pid: u32) -> Option<u64> {
        // After the command name: the 20th field is the start time, in clock ticks after boot.
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        rest.split_whitespace().nth(19)?.parse().ok()
    }

    pub fn machine_id() -> Option<String> {
        ["/etc/machine-id", "/var/lib/dbus/machine-id"]
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .map(|s| s.trim().to_string())
            .find(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
    }

    pub fn boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or_default()
    }

    pub fn ppid(pid: u32) -> Option<u32> {
        // After the command name (in parentheses, any bytes): state, then the parent's id.
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        rest.split_whitespace().nth(1)?.parse().ok()
    }

    pub fn socket_peer(fd: std::os::unix::io::RawFd) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: SO_PEERCRED writes at most `len` bytes into `cred`.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((cred.pid, cred.uid))
    }

    pub fn running(pid: libc::pid_t) -> bool {
        // The state follows the command name, which is in parentheses and may hold any byte.
        let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else { return false };
        let Some(end) = stat.iter().rposition(|b| *b == b')') else { return false };
        stat[end + 1..].iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'Z')
    }

    pub fn session_members(sid: libc::pid_t) -> Vec<libc::pid_t> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        dir.filter_map(|e| e.ok())
            .filter_map(|e| {
                let pid: libc::pid_t = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read(e.path().join("stat")).ok()?;
                let end = stat.iter().rposition(|b| *b == b')')?;
                // After the name: state, parent, process group, session.
                let fields: Vec<&[u8]> =
                    stat[end + 1..].split(u8::is_ascii_whitespace).filter(|f| !f.is_empty()).take(4).collect();
                let session = std::str::from_utf8(fields.get(3)?).ok()?.parse::<libc::pid_t>().ok()?;
                (session == sid && !matches!(fields[0], b"Z" | b"X")).then_some(pid)
            })
            .collect()
    }

    pub fn group_running(pgid: libc::pid_t) -> bool {
        let Ok(dir) = std::fs::read_dir("/proc") else { return true };
        dir.filter_map(|e| e.ok()).any(|e| {
            if !e.file_name().as_encoded_bytes().iter().all(u8::is_ascii_digit) {
                return false;
            }
            let Ok(stat) = std::fs::read(e.path().join("stat")) else { return false };
            let Some(end) = stat.iter().rposition(|b| *b == b')') else { return false };
            // After the name: state, parent, process group.
            let mut fields = stat[end + 1..].split(u8::is_ascii_whitespace).filter(|f| !f.is_empty());
            let (Some(state), _, Some(group)) = (fields.next(), fields.next(), fields.next()) else { return false };
            !matches!(state, b"Z" | b"X")
                && std::str::from_utf8(group).ok().and_then(|g| g.parse::<libc::pid_t>().ok()) == Some(pgid)
        })
    }

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
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    /// `struct proc_fileinfo` and `struct vnode_fdinfowithpath` of `<sys/proc_info.h>`, which
    /// libc does not have.
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }
    #[repr(C)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: libc::vnode_info_path,
    }
    /// `PROC_PIDFDVNODEPATHINFO`.
    const PIDFD_VNODE_PATH: libc::c_int = 2;
    // The size the kernel checks the buffer against (`PROC_PIDFDVNODEPATHINFO_SIZE`): 24 bytes
    // of file info, then the 1176 of `vnode_info_path`.
    const _: () = assert!(std::mem::size_of::<VnodeFdInfoWithPath>() == 1200);

    /// The paths of the files process `pid` holds open (vnodes: files and directories).
    pub fn open_files(pid: u32) -> Vec<PathBuf> {
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer only asks for the size needed.
        let need = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        // Room for a few more opened meanwhile.
        let mut fds: Vec<libc::proc_fdinfo> =
            (0..need as usize / entry + 16).map(|_| libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }).collect();
        let size = (fds.len() * entry) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long.
        let n =
            unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
        fds.truncate(n.max(0) as usize / entry);
        fds.iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
            .filter_map(|f| {
                let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int;
                // SAFETY: proc_pidfdinfo writes at most `size` bytes into `info`.
                let n = unsafe {
                    libc::proc_pidfdinfo(
                        pid as libc::c_int,
                        f.proc_fd,
                        PIDFD_VNODE_PATH,
                        (&mut info as *mut VnodeFdInfoWithPath).cast::<libc::c_void>(),
                        size,
                    )
                };
                if n != size {
                    return None;
                }
                let bytes: Vec<u8> =
                    info.pvip.vip_path.iter().flatten().take_while(|c| **c != 0).map(|c| *c as u8).collect();
                (!bytes.is_empty()).then(|| PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
            })
            .collect()
    }
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    pub fn args_and_env(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut size: libc::size_t = 0;
        // SAFETY: sysctl with a null buffer only reports the size needed.
        let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) };
        if rc != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the buffer is `size` bytes long, as the kernel asked.
        let rc =
            unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return None;
        }
        buf.truncate(size);
        parse_procargs2(&buf)
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast::<libc::c_void>(),
                size,
            )
        };
        if n != size {
            return None;
        }
        // libc spells the MAXPATHLEN buffer as 32 rows of 32 characters.
        let bytes: Vec<u8> =
            info.pvi_cdir.vip_path.iter().flatten().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        (!bytes.is_empty()).then(|| PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn socket_peer(fd: std::os::unix::io::RawFd) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: LOCAL_PEERPID writes at most `len` bytes into `pid`.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast::<libc::c_void>(),
                &mut len,
            )
        };
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid only writes the two ids.
        if rc != 0 || unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((pid, uid))
    }

    pub fn running(pid: libc::pid_t) -> bool {
        // A zombie has ended: only its exit status is left, for its parent to read. When the
        // kernel says nothing about the process (another user's), it counts as running.
        exists(pid) && short_info(pid).is_none_or(|info| info.pbsi_status != libc::SZOMB)
    }

    /// The kernel's short description of a process (state, group, session), when it gives one.
    fn short_info(pid: libc::pid_t) -> Option<libc::proc_bsdshortinfo> {
        // SAFETY: an all-zero proc_bsdshortinfo is valid; proc_pidinfo writes at most `size` bytes.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdshortinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (n == size).then_some(info)
    }

    pub fn session_members(sid: libc::pid_t) -> Vec<libc::pid_t> {
        let mut pids = vec![0 as libc::pid_t; 8192];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long; unused entries stay 0.
        if unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) } <= 0 {
            return Vec::new();
        }
        pids.into_iter()
            .filter(|p| *p > 0)
            // SAFETY: getsid only reads the process table.
            .filter(|p| unsafe { libc::getsid(*p) } == sid)
            .filter(|p| short_info(*p).is_some_and(|info| info.pbsi_status != libc::SZOMB))
            .collect()
    }

    pub fn group_running(pgid: libc::pid_t) -> bool {
        let mut pids = vec![0 as libc::pid_t; 1024];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long; unused entries stay 0.
        if unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) } <= 0 {
            return true;
        }
        pids.into_iter().filter(|p| *p > 0).any(|pid| {
            let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
            // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
            let n = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDT_SHORTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdshortinfo).cast::<libc::c_void>(),
                    size,
                )
            };
            n == size && info.pbsi_status != libc::SZOMB
        })
    }

    pub fn start_time(pid: u32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn machine_id() -> Option<String> {
        let mut id = [0u8; 16];
        let wait = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: gethostuuid writes the 16 bytes of the host's UUID.
        let rc = unsafe { libc::gethostuuid(id.as_mut_ptr(), &wait) };
        (rc == 0).then(|| id.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn boot_id() -> String {
        String::new()
    }

    pub fn pids() -> Vec<u32> {
        // SAFETY: a null buffer only asks how many there are.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; n as usize + 64];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
        pids.truncate(n.max(0) as usize);
        pids.into_iter().filter(|p| *p > 0).map(|p| p as u32).collect()
    }

    pub fn ppid(pid: u32) -> Option<u32> {
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdshortinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (n == size).then_some(info.pbsi_ppid)
    }

    /// `lsof` is slow: it runs on its own thread, at most every two seconds, and callers get
    /// the last answer at once (empty until the first one).
    pub fn listening_ports() -> HashSet<u16> {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CACHE: Mutex<Option<(Instant, HashSet<u16>)>> = Mutex::new(None);
        static ASKING: AtomicBool = AtomicBool::new(false);
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = cache.as_ref().is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(2));
        let ports = cache.as_ref().map(|(_, p)| p.clone()).unwrap_or_default();
        drop(cache);
        if !fresh && !ASKING.swap(true, Ordering::SeqCst) {
            std::thread::spawn(|| {
                let mut cmd = std::process::Command::new("lsof");
                cmd.args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fn"]);
                let out = crate::exec::run(&mut cmd, Duration::from_secs(5), 1 << 20)
                    .map(|o| o.stdout_text())
                    .unwrap_or_default();
                *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), parse_lsof(&out)));
                ASKING.store(false, Ordering::SeqCst);
            });
        }
        ports
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;

    /// Open files are not looked at here: only working directories count.
    pub fn open_files(_pid: u32) -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn args_and_env(_pid: u32) -> Option<(Vec<String>, Vec<String>)> {
        None
    }

    pub fn cwd(_pid: u32) -> Option<PathBuf> {
        None
    }

    pub fn socket_peer(_fd: std::os::unix::io::RawFd) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    pub fn running(pid: libc::pid_t) -> bool {
        exists(pid)
    }

    /// Without a way to tell zombies apart: any member counts.
    pub fn start_time(_pid: u32) -> Option<u64> {
        None
    }

    pub fn machine_id() -> Option<String> {
        None
    }

    pub fn boot_id() -> String {
        String::new()
    }

    pub fn pids() -> Vec<u32> {
        Vec::new()
    }

    pub fn ppid(_pid: u32) -> Option<u32> {
        None
    }

    pub fn group_running(_pgid: libc::pid_t) -> bool {
        true
    }

    pub fn session_members(_sid: libc::pid_t) -> Vec<libc::pid_t> {
        Vec::new()
    }

    pub fn listening_ports() -> HashSet<u16> {
        HashSet::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process() {
        let me = std::process::id();
        let argv = argv(me).expect("own command line");
        assert!(!argv.is_empty());
        assert_eq!(cwd(me).unwrap(), std::env::current_dir().unwrap().canonicalize().unwrap());
        // The environment as the process started (other tests change PATH while running).
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(environ(me, &["HOME"]).get("HOME"), Some(&home));
        }
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // macOS answers from a cache that `lsof` fills in the background.
        let start = std::time::Instant::now();
        while !listening_ports().contains(&port) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(listening_ports().contains(&port));
    }

    #[test]
    fn a_group_left_with_its_ended_leader_runs_nothing() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.3"]).process_group(0).spawn().unwrap();
        let pgid = child.id() as libc::pid_t;
        assert!(group_running(pgid));
        // Ended, not reaped: the group still has its zombie leader, and nothing running.
        let start = std::time::Instant::now();
        while group_running(pgid) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: signal 0 only checks the group still has a member. Linux counts the zombie
        // leader as one; macOS does not.
        if cfg!(target_os = "linux") {
            assert_eq!(unsafe { libc::kill(-pgid, 0) }, 0, "the zombie is still in its group");
        }
        assert!(!group_running(pgid));
        child.wait().unwrap();
        assert!(!group_running(pgid));
    }

    #[test]
    fn socket_peer_and_running() {
        let (a, b) = UnixStream::pair().unwrap();
        let (pid, uid) = socket_peer(&a).unwrap();
        assert_eq!(pid as u32, std::process::id());
        assert_eq!(uid, unsafe { libc::getuid() });
        drop(b);
        assert!(running(pid));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let child_pid = child.id() as libc::pid_t;
        // Exited but not reaped yet: told apart from a running process.
        let start = std::time::Instant::now();
        while running(child_pid) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!running(child_pid), "a zombie does not run");
        child.wait().unwrap();
        assert!(!running(child_pid));
    }

    #[test]
    fn what_works_in_a_directory_is_found() {
        let dir = std::env::temp_dir().join(format!("kraal-working-in-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        // In its own process group, as a build an agent started would be.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .current_dir(dir.join("deep"))
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = std::time::Instant::now();
        while !working_in(&dir).iter().any(|(p, _)| *p == pid) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let found = working_in(&dir);
        assert!(found.iter().any(|(p, name)| *p == pid && name == "sleep"), "{found:?}");
        assert!(!working_in(&dir.join("other")).iter().any(|(p, _)| *p == pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!working_in(&dir).iter().any(|(p, _)| *p == pid));
        // Elsewhere, but writing there through an absolute path (Linux sees open files).
        if cfg!(target_os = "linux") {
            let log = dir.join("deep/build.log");
            let mut child = std::process::Command::new("sh")
                .args(["-c", &format!("exec 3>>'{}'; exec sleep 5", log.display())])
                .current_dir(std::env::temp_dir())
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = child.id();
            let start = std::time::Instant::now();
            while !working_in(&dir).iter().any(|(p, _)| *p == pid)
                && start.elapsed() < std::time::Duration::from_secs(5)
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(working_in(&dir).iter().any(|(p, _)| *p == pid), "a file held open there counts");
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nul_separated() {
        assert_eq!(nul_strings(b"a\0bc\0\0"), vec!["a", "bc"]);
    }

    #[test]
    fn macos_formats() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/zsh\0\0\0\0zsh\0-l\0HOME=/Users/me\0TERM=xterm\0\0garbage");
        let (argv, env) = parse_procargs2(&buf).unwrap();
        assert_eq!(argv, vec!["zsh", "-l"]);
        assert_eq!(env, vec!["HOME=/Users/me", "TERM=xterm"]);
        let ports = parse_lsof("p123\nf5\nn*:5173\nn127.0.0.1:8000\nn[::1]:9000\n");
        assert_eq!(ports, [5173, 8000, 9000].into_iter().collect());
    }
}
