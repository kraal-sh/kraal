//! Where the tests run: a home, a runtime directory and a `PATH` that are not the developer's.
//!
//! The test process changes its own environment before its tests start, so that whatever a test
//! starts, however it starts it, inherits directories of the tests' own: nothing then reads the
//! developer's dotfiles or accounts, runs their agents, or reaches the kraal session the tests
//! may be run from. The unit tests and the end-to-end tests share this file.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Names to a kraal the tests run (a debug build) the directories it must stay out of, as
/// `PATH` names its own: one that would use something there says so and stops, where it would
/// have passed or failed on what that developer has.
pub const KEEP_OUT: &str = "KRAAL_TEST_KEEP_OUT";

/// Programs a test reaches only where it put one of its own: the developer's agents are signed
/// in to their accounts, their kraal is not the one under test, and clipboard tools would
/// change their desktop clipboard.
const THEIRS: [&str; 7] = ["claude", "codex", "kraal", "pbcopy", "wl-copy", "xclip", "xsel"];

/// The directory of this test process and its pid, set by [`isolate`].
static ROOT: OnceLock<(PathBuf, u32)> = OnceLock::new();

/// Runs [`isolate`] when the test binary is loaded: before its tests, and while it has one
/// thread, which is when a process may change its own environment.
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".init_array"))]
static ISOLATE: extern "C" fn() = isolate;

extern "C" fn isolate() {
    let pid = std::process::id();
    // Under /tmp: a socket path is short, and macOS's temporary directory is not.
    let root = PathBuf::from("/tmp").join(format!("kraal-tests-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    let (home, run) = (root.join("home"), root.join("run"));
    for dir in [&home, &run] {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    }
    // A test binary that a test starts is told by the first what to stay out of, and has its
    // `PATH` already.
    let told = std::env::var_os(KEEP_OUT);
    let real_home = if told.is_some() { None } else { dirs::home_dir().filter(|home| home.parent().is_some()) };
    let keep_out = told.unwrap_or_else(|| developers_directories(real_home.as_deref()));
    let path = path(&std::env::var_os("PATH").unwrap_or_default(), real_home.as_deref(), &root.join("path"));
    let theirs: Vec<OsString> = std::env::vars_os().map(|(name, _)| name).filter(|name| is_theirs(name)).collect();
    // SAFETY: this process has one thread, so nothing reads the environment meanwhile; and
    // `remove_root` is a function that may run as the process ends.
    unsafe {
        for name in theirs {
            std::env::remove_var(name);
        }
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_RUNTIME_DIR", &run);
        std::env::set_var("PATH", path);
        std::env::set_var(KEEP_OUT, keep_out);
        libc::atexit(remove_root);
    }
    let _ = ROOT.set((root, pid));
}

extern "C" fn remove_root() {
    // A child that ends without having run another program is not the process this belongs to.
    if let Some((root, pid)) = ROOT.get()
        && *pid == std::process::id()
    {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// What is the developer's: their home, their runtime directory, and the directory every kraal
/// without a runtime directory shares (`kraal-<uid>` in a temporary directory): where their
/// files and their sessions are.
fn developers_directories(home: Option<&Path>) -> OsString {
    // SAFETY: getuid cannot fail.
    let shared = format!("kraal-{}", unsafe { libc::getuid() });
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|dir| dir.is_absolute());
    let temporary = [PathBuf::from("/tmp"), std::env::temp_dir()].map(|dir| dir.join(&shared));
    let dirs = home.map(Path::to_path_buf).into_iter().chain(runtime).chain(temporary);
    std::env::join_paths(dirs).expect("directories that a PATH can name")
}

/// Whether variable `name` is the developer's own: what their shell, a git hook or the kraal
/// pane the tests are run in has set, which would send a program to their files, their accounts
/// or their session. `KRAAL_OLD` and `KRAAL_FUZZ_*` are said to the tests themselves.
fn is_theirs(name: &OsStr) -> bool {
    const NAMES: [&str; 19] = [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "WAYLAND_SOCKET",
        "XAUTHORITY",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "ZDOTDIR",
        "ENV",
        "BASH_ENV",
        "SSH_AUTH_SOCK",
        "KUBECONFIG",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "TF_WORKSPACE",
        "TF_DATA_DIR",
    ];
    let Some(name) = name.to_str() else { return false };
    let kraals = name == "KRAAL" || name.starts_with("KRAAL_");
    NAMES.contains(&name)
        || name.starts_with("GIT_")
        || (kraals && name != "KRAAL_OLD" && name != KEEP_OUT && !name.starts_with("KRAAL_FUZZ_"))
}

/// `theirs`, the developer's `PATH`, as the tests have it: without the directories of their
/// home, and without the programs that are theirs alone, a directory that holds one being
/// replaced by one under `links` that links to everything else in it.
pub fn path(theirs: &OsStr, real_home: Option<&Path>, links: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(theirs) {
        if dir.is_absolute() && !dirs.contains(&dir) && !real_home.is_some_and(|home| within(&dir, home)) {
            dirs.push(dir);
        }
    }
    let dirs = dirs.into_iter().enumerate().map(|(n, dir)| {
        let holds_one = THEIRS.iter().any(|program| dir.join(program).symlink_metadata().is_ok());
        if holds_one { linked(&dir, &links.join(n.to_string())) } else { dir }
    });
    std::env::join_paths(dirs).expect("directories that a PATH named")
}

/// Whether `path` is `dir` or in it, as written or once links are followed.
fn within(path: &Path, dir: &Path) -> bool {
    let followed = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    path.starts_with(dir) || followed(path).starts_with(followed(dir))
}

/// A directory `links` of links to what `dir` holds, the developer's own programs left out.
fn linked(dir: &Path, links: &Path) -> PathBuf {
    std::fs::create_dir_all(links).unwrap_or_else(|e| panic!("{}: {e}", links.display()));
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if !THEIRS.iter().any(|program| name == *program) {
            std::os::unix::fs::symlink(entry.path(), links.join(&name))
                .unwrap_or_else(|e| panic!("{}: {e}", links.join(&name).display()));
        }
    }
    links.to_path_buf()
}

/// Fail the test that calls this when the tests are not in directories of their own: the
/// environment was not changed before they started, or something has put the developer's back.
pub fn check() {
    let (root, _) = ROOT.get().expect("the environment of the tests was not set before they started");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    assert_eq!(home.as_deref(), Some(root.join("home").as_path()), "HOME is no longer the tests' own");
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    assert_eq!(run.as_deref(), Some(root.join("run").as_path()), "XDG_RUNTIME_DIR is no longer the tests' own");
}
