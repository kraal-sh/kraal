//! What a pane works on besides its directory and server: the Kubernetes context, the Terraform
//! workspace and the AWS profile, and whether any of it is production.
//!
//! Everything here is read from local files and `/proc`; kraal never runs kubectl, terraform or
//! aws itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Programs that act on the current Kubernetes context.
const KUBE_TOOLS: &[&str] = &[
    "kubectl",
    "k9s",
    "helm",
    "helmfile",
    "stern",
    "kubectx",
    "kubens",
    "kustomize",
    "flux",
    "argocd",
    "oc",
    "skaffold",
    "tilt",
    "kubecolor",
    "kubeseal",
    "velero",
    "istioctl",
    "linkerd",
];

/// Programs that act on the current AWS profile.
const AWS_TOOLS: &[&str] =
    &["aws", "terraform", "tofu", "terragrunt", "cdk", "sam", "eksctl", "pulumi", "copilot", "packer"];

/// A program that runs another one named among its arguments (`watch kubectl get pods`,
/// `sudo -u ops helm …`, `python3 /usr/bin/aws`): what its options are, so that an option's
/// value is never taken for the program.
struct Wrapper {
    name: &'static str,
    /// Options followed by a value (`-n 2`).
    values: &'static [&'static str],
    /// Options after which what runs is code given inline (`sh -c …`, `python3 -m …`), or a
    /// mode where no program follows (`ionice -p PID`): kraal cannot name a program then.
    no_program: &'static [&'static str],
    /// Operands before the program (`timeout 5 kubectl …`: the duration).
    operands: usize,
    /// `NAME=value` arguments before the program set its environment.
    assignments: bool,
}

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "watch",
        values: &["-n", "--interval", "-q", "--equexit", "-s", "--shotsdir"],
        no_program: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        name: "viddy",
        values: &["-n", "--interval", "--shell", "--shell-options", "--max-history"],
        no_program: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        name: "sudo",
        values: &[
            "-a",
            "-C",
            "-c",
            "-D",
            "-g",
            "-h",
            "-p",
            "-R",
            "-r",
            "-T",
            "-t",
            "-U",
            "-u",
            "--auth-type",
            "--close-from",
            "--chdir",
            "--group",
            "--host",
            "--login-class",
            "--prompt",
            "--chroot",
            "--role",
            "--type",
            "--command-timeout",
            "--other-user",
            "--user",
        ],
        // `-k` only forgets the cached password: with a command, the command runs.
        no_program: &["-e", "--edit", "-l", "--list", "-v", "--validate", "-K", "--remove-timestamp"],
        operands: 0,
        assignments: true,
    },
    Wrapper { name: "doas", values: &["-u", "-C"], no_program: &[], operands: 0, assignments: false },
    Wrapper {
        name: "env",
        values: &["-u", "--unset", "-C", "--chdir"],
        // `env -S 'kubectl -n prod'`: the program is inside one string.
        no_program: &["-S", "--split-string"],
        operands: 0,
        assignments: true,
    },
    Wrapper {
        name: "time",
        values: &["-f", "--format", "-o", "--output"],
        no_program: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper { name: "nice", values: &["-n", "--adjustment"], no_program: &[], operands: 0, assignments: false },
    Wrapper { name: "nohup", values: &[], no_program: &[], operands: 0, assignments: false },
    Wrapper {
        name: "timeout",
        values: &["-k", "--kill-after", "-s", "--signal"],
        no_program: &[],
        operands: 1,
        assignments: false,
    },
    Wrapper {
        name: "stdbuf",
        values: &["-i", "-o", "-e", "--input", "--output", "--error"],
        no_program: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        name: "ionice",
        values: &["-c", "--class", "-n", "--classdata"],
        no_program: &["-p", "--pid", "-P", "--pgid", "-u", "--uid"],
        operands: 0,
        assignments: false,
    },
    Wrapper { name: "python", values: &["-W", "-X"], no_program: &["-c", "-m"], operands: 0, assignments: false },
    Wrapper { name: "python3", values: &["-W", "-X"], no_program: &["-c", "-m"], operands: 0, assignments: false },
    Wrapper {
        name: "node",
        values: &["-r", "--require", "--import", "--loader", "--experimental-loader", "-C", "--conditions"],
        no_program: &["-e", "--eval", "-p", "--print"],
        operands: 0,
        assignments: false,
    },
    Wrapper { name: "sh", values: &["-o", "+o"], no_program: &["-c", "-s"], operands: 0, assignments: false },
    Wrapper { name: "dash", values: &["-o", "+o"], no_program: &["-c", "-s"], operands: 0, assignments: false },
    Wrapper {
        name: "bash",
        values: &["-o", "+o", "-O", "+O", "--rcfile", "--init-file"],
        no_program: &["-c", "-s"],
        operands: 0,
        assignments: false,
    },
    Wrapper { name: "zsh", values: &["-o", "+o"], no_program: &["-c", "-s"], operands: 0, assignments: false },
];

fn base(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

/// What one argument of a wrapper is.
enum WrapperArg {
    /// An option, or several short ones together, followed by `taken` more arguments (their
    /// value, when it is not attached).
    Option { taken: usize },
    /// Code or a mode with no program after it.
    NoProgram,
    /// Not an option.
    Operand,
}

impl Wrapper {
    fn arg(&self, a: &str) -> WrapperArg {
        let option = (a.starts_with('-') || (a.starts_with('+') && self.values.contains(&"+o"))) && a.len() > 1;
        if !option {
            return WrapperArg::Operand;
        }
        if let Some(long) = a.strip_prefix("--") {
            // `--name=value` carries its value.
            let name = format!("--{}", long.split('=').next().unwrap_or(long));
            if self.no_program.contains(&name.as_str()) {
                return WrapperArg::NoProgram;
            }
            let taken = usize::from(self.values.contains(&name.as_str()) && !a.contains('='));
            return WrapperArg::Option { taken };
        }
        // Short options can come together (`bash -lc …`, `watch -tn 2 …`): the first that takes
        // a value takes the rest of the argument, or the next one.
        let sign = &a[..1];
        let letters: Vec<char> = a[1..].chars().collect();
        for (i, c) in letters.iter().enumerate() {
            let short = format!("{sign}{c}");
            if self.no_program.contains(&short.as_str()) {
                return WrapperArg::NoProgram;
            }
            if self.values.contains(&short.as_str()) {
                return WrapperArg::Option { taken: usize::from(i + 1 == letters.len()) };
            }
        }
        WrapperArg::Option { taken: 0 }
    }
}

/// Where the program among `tools` is in a command line: first, or after wrappers, their
/// options with their values, their operands and `env`'s assignments. When a wrapper is given
/// code to run (`sh -c …`, `python3 -m …`), there is no program kraal can name.
fn tool_index(argv: &[String], tools: &[&str]) -> Option<usize> {
    let mut i = 0;
    loop {
        let program = base(argv.get(i)?);
        if tools.contains(&program) {
            return Some(i);
        }
        let wrapper = WRAPPERS.iter().find(|w| w.name == program)?;
        i += 1;
        let mut operands = wrapper.operands;
        while let Some(a) = argv.get(i) {
            if a == "--" {
                i += 1;
                break;
            }
            match wrapper.arg(a) {
                WrapperArg::NoProgram => return None,
                WrapperArg::Option { taken } => i += 1 + taken,
                WrapperArg::Operand if wrapper.assignments && is_assignment(a) => i += 1,
                WrapperArg::Operand if operands > 0 => {
                    operands -= 1;
                    i += 1;
                }
                WrapperArg::Operand => break,
            }
        }
    }
}

/// `NAME=value`, as `env` and `sudo` take before the program.
fn is_assignment(a: &str) -> bool {
    a.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Whether a command line runs one of `tools`, directly or through a wrapper.
fn runs(argv: &[String], tools: &[&str]) -> bool {
    tool_index(argv, tools).is_some()
}

pub fn runs_kube_tool(argv: &[String]) -> bool {
    runs(argv, KUBE_TOOLS)
}

pub fn runs_aws_tool(argv: &[String]) -> bool {
    runs(argv, AWS_TOOLS)
}

const TERRAFORM: &[&str] = &["terraform", "tofu", "terragrunt"];

pub fn runs_terraform(argv: &[String]) -> bool {
    runs(argv, TERRAFORM)
}

/// The arguments of the program among `tools` that a command line runs, after wrappers and
/// their options, up to `--`: where that program's flags are. In
/// `watch -n 2 kubectl -n prod get pods`, `-n 2` is watch's.
fn tool_args<'a>(argv: &'a [String], tools: &[&str]) -> &'a [String] {
    let Some(i) = tool_index(argv, tools) else { return &[] };
    let args = &argv[i + 1..];
    &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())]
}

/// The value of `--name X` or `--name=X` (or a short `-n X`) among a program's arguments.
fn flag(argv: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in argv.iter().enumerate() {
        for name in names {
            if a == name {
                return argv.get(i + 1).cloned();
            }
            if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// What the last tools run in a pane worked on.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Absolute `KUBECONFIG` of the last Kubernetes tool ("" for the default file), and the
    /// context and namespace it chose by flags.
    pub kubeconfig: Option<String>,
    pub kube_flags: KubeFlags,
    /// The last AWS tool's profile: `Some(None)` when it used the default.
    pub aws_profile: Option<Option<String>>,
    /// The workspace the last Terraform run worked on.
    pub tf_workspace: Option<TfSeen>,
}

/// Where a Terraform run's workspace came from, which decides how long it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TfSeen {
    /// `TF_WORKSPACE` in its environment: the shell's, wherever it goes next.
    Env(String),
    /// Selected in the directory it worked on (`dir`: `-chdir`, with `TF_DATA_DIR`), read again
    /// off the event loop as long as the pane stays in the directory it ran from (`from`).
    Dir { from: PathBuf, dir: PathBuf, data_dir: Option<String> },
}

/// What a Kubernetes command line chose by itself, overriding the kubeconfig's choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KubeFlags {
    pub context: Option<String>,
    pub namespace: Option<String>,
    pub kubeconfig: Option<String>,
}

pub fn kube_flags(argv: &[String]) -> KubeFlags {
    let args = tool_args(argv, KUBE_TOOLS);
    KubeFlags {
        context: flag(args, &["--context", "--kube-context"]),
        namespace: flag(args, &["--namespace", "-n"]),
        kubeconfig: flag(args, &["--kubeconfig"]),
    }
}

/// `--profile X` of an AWS command line.
pub fn aws_profile_flag(argv: &[String]) -> Option<String> {
    flag(tool_args(argv, AWS_TOOLS), &["--profile"])
}

// ---- Kubernetes --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeContext {
    pub context: String,
    pub namespace: Option<String>,
}

impl KubeContext {
    /// `⎈ prod-eks/payments` style label, with long cloud context names shortened.
    pub fn label(&self, ascii: bool) -> String {
        let mut s = format!("{}{}", if ascii { "k8s:" } else { "⎈ " }, short_context(&self.context));
        if let Some(ns) = self.namespace.as_deref().filter(|n| *n != "default") {
            s.push('/');
            s.push_str(ns);
        }
        s
    }
}

/// `arn:aws:eks:eu-west-1:123:cluster/prod` → `prod`; `gke_project_zone_prod` → `prod`.
pub fn short_context(ctx: &str) -> String {
    let short = if ctx.starts_with("arn:") {
        ctx.rsplit('/').next().unwrap_or(ctx)
    } else if let Some(rest) = ctx.strip_prefix("gke_") {
        rest.rsplit('_').next().unwrap_or(ctx)
    } else {
        ctx
    };
    if short.chars().count() > 28 { format!("{}…", short.chars().take(27).collect::<String>()) } else { short.into() }
}

/// A `KUBECONFIG` value with relative entries made absolute against `base` (the directory of
/// the process that used it), so it means the same thing to kraal.
pub fn absolute_kubeconfig(value: &str, base: Option<&Path>) -> String {
    value
        .split(':')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let path = crate::paths::expand_tilde(p);
            match base {
                Some(b) if path.is_relative() => b.join(path).display().to_string(),
                _ => path.display().to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// The kubeconfig files a `KUBECONFIG` value names; the default file when it is empty.
pub fn kubeconfig_paths(value: &str) -> Vec<PathBuf> {
    let paths: Vec<PathBuf> = value.split(':').filter(|p| !p.is_empty()).map(crate::paths::expand_tilde).collect();
    if !paths.is_empty() {
        return paths;
    }
    dirs::home_dir().map(|h| vec![h.join(".kube").join("config")]).unwrap_or_default()
}

/// What a kubeconfig file says: its current context, and each context's namespace.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KubeFile {
    pub current: Option<String>,
    pub namespaces: HashMap<String, Option<String>>,
    /// The file exists but could not be read or parsed: what its context is, is unknown.
    pub unreadable: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RawKubeconfig {
    #[serde(rename = "current-context")]
    current_context: Option<String>,
    contexts: Option<Vec<RawContext>>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RawContext {
    name: Option<String>,
    context: Option<RawContextBody>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RawContextBody {
    namespace: Option<String>,
}

/// Read the parts of a kubeconfig kraal shows (YAML or JSON, as kubectl takes).
pub fn parse_kubeconfig(text: &str) -> KubeFile {
    if text.trim().is_empty() {
        return KubeFile::default();
    }
    let raw: RawKubeconfig = match serde_yaml_ng::from_str(text) {
        Ok(r) => r,
        Err(e) => return KubeFile { unreadable: Some(e.to_string()), ..Default::default() },
    };
    let mut file = KubeFile { current: raw.current_context.filter(|c| !c.is_empty()), ..Default::default() };
    for c in raw.contexts.unwrap_or_default() {
        let Some(name) = c.name else { continue };
        let ns = c.context.and_then(|b| b.namespace).filter(|n| !n.is_empty());
        file.namespaces.entry(name).or_insert(ns);
    }
    file
}

/// Kubeconfig files merged like kubectl does: the first file that sets `current-context` wins,
/// and so does the first definition of a context.
pub fn merge_kubeconfigs(files: &[KubeFile]) -> KubeFile {
    let mut merged = KubeFile {
        current: files.iter().find_map(|f| f.current.clone()),
        // kubectl refuses to work with a broken file in the list.
        unreadable: files.iter().find_map(|f| f.unreadable.clone()),
        ..Default::default()
    };
    for f in files {
        for (name, ns) in &f.namespaces {
            merged.namespaces.entry(name.clone()).or_insert_with(|| ns.clone());
        }
    }
    merged
}

impl KubeFile {
    /// The context a command works on: the one its flags name, else the current one.
    pub fn resolve(&self, flags: &KubeFlags) -> Option<KubeContext> {
        let context = flags.context.clone().or_else(|| self.current.clone())?;
        let namespace = flags.namespace.clone().or_else(|| self.namespaces.get(&context).cloned().flatten());
        Some(KubeContext { context, namespace })
    }
}

const KUBECONFIG_MAX: u64 = 16 << 20;

/// A kubeconfig as read, with the modification time it had then.
type Cached = (Option<std::time::SystemTime>, KubeFile);

/// Kubeconfig files read once per change of their modification time. Clones share the cache,
/// so that it can be refreshed off the event loop: each `KUBECONFIG` value on its own, one read
/// at a time, so that a file that does not answer (a stalled network disk) holds up only the
/// panes that use it.
#[derive(Default, Clone)]
pub struct KubeCache {
    files: Arc<Mutex<HashMap<PathBuf, Cached>>>,
    /// `KUBECONFIG` values being read: since when, and the read's number.
    reading: Arc<Mutex<HashMap<String, (Instant, u64)>>>,
    /// Reads are numbered: a context read twice the same way can be told from one that changed
    /// in between, or that is read again (what a production approval was given for).
    numbers: Arc<std::sync::atomic::AtomicU64>,
    /// For each value, the number of the last read told, and of the read since which what is
    /// known of it holds.
    told: Arc<Mutex<HashMap<String, (u64, u64)>>>,
}

impl KubeCache {
    /// Start reading `kubeconfig`, unless it is being read: then `None`. Whoever gets a number
    /// calls [`KubeCache::end`] when done.
    pub fn begin(&self, kubeconfig: &str) -> Option<u64> {
        let mut reading = self.reading.lock().unwrap_or_else(PoisonError::into_inner);
        if reading.contains_key(kubeconfig) {
            return None;
        }
        let number = self.number();
        reading.insert(kubeconfig.to_string(), (Instant::now(), number));
        Some(number)
    }

    pub fn end(&self, kubeconfig: &str) {
        self.reading.lock().unwrap_or_else(PoisonError::into_inner).remove(kubeconfig);
    }

    /// A number for a read made outside [`KubeCache::begin`].
    pub fn number(&self) -> u64 {
        1 + self.numbers.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Since when `kubeconfig` is being read, and that read's number, if it is.
    pub fn reading(&self, kubeconfig: &str) -> Option<(Instant, u64)> {
        self.reading.lock().unwrap_or_else(PoisonError::into_inner).get(kubeconfig).copied()
    }

    /// Whether read `number` of `kubeconfig` is newer than what was told of it, and if so, take
    /// it as told, with `changed` saying whether what it found differs from before: reads that
    /// end out of order never bring back an older state.
    pub fn told(&self, kubeconfig: &str, number: u64, changed: bool) -> bool {
        let mut told = self.told.lock().unwrap_or_else(PoisonError::into_inner);
        let (last, since) = told.entry(kubeconfig.to_string()).or_insert((0, 0));
        if number <= *last {
            return false;
        }
        *last = number;
        if changed || *since == 0 {
            *since = number;
        }
        true
    }

    /// The number of the read since which what is known of `kubeconfig` holds.
    pub fn known_since(&self, kubeconfig: &str) -> Option<u64> {
        self.told.lock().unwrap_or_else(PoisonError::into_inner).get(kubeconfig).map(|(_, since)| *since)
    }

    /// Forget what was told of values no pane uses any more.
    pub fn retain_told(&self, keep: impl Fn(&str) -> bool) {
        self.told.lock().unwrap_or_else(PoisonError::into_inner).retain(|v, _| keep(v));
    }

    /// Read `kubeconfig` now (a tool is about to act on it), giving up after `limit`: a file
    /// that does not answer by then is unknown (`None`); its read goes on and is not waited for.
    pub fn read_within(&self, kubeconfig: &str, limit: std::time::Duration) -> Option<(u64, KubeFile)> {
        let number = self.number();
        let (cache, value) = (self.clone(), kubeconfig.to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("kubeconfig".into())
            .spawn(move || {
                let _ = tx.send(cache.merged(&value));
            })
            .ok()?;
        rx.recv_timeout(limit).ok().map(|f| (number, f))
    }

    /// The kubeconfig a `KUBECONFIG` value names, read and merged.
    pub fn merged(&self, kubeconfig: &str) -> KubeFile {
        let paths = kubeconfig_paths(kubeconfig);
        let files: Vec<KubeFile> = paths.iter().map(|p| self.file(p)).collect();
        merge_kubeconfigs(&files)
    }

    /// One file, read again when its modification time changed. The cache is not held while
    /// the file is read: a slow file must not hold up the others.
    fn file(&self, path: &Path) -> KubeFile {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some((t, f)) = self.files.lock().unwrap_or_else(PoisonError::into_inner).get(path)
            && *t == mtime
        {
            return f.clone();
        }
        #[cfg(debug_assertions)]
        slow_for_tests(path);
        // A kubeconfig is a small text file; anything else (a FIFO, a device) is unknown.
        let f = match crate::paths::read_regular_text(path, KUBECONFIG_MAX) {
            Ok(t) => parse_kubeconfig(&t),
            // No file is no configuration: kubectl has no context either.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => KubeFile::default(),
            Err(e) => KubeFile { unreadable: Some(format!("{}: {e}", path.display())), ..Default::default() },
        };
        self.files.lock().unwrap_or_else(PoisonError::into_inner).insert(path.to_path_buf(), (mtime, f.clone()));
        f
    }
}

/// Tests (debug builds only) make the read of a kubeconfig, or of a Terraform `environment` file,
/// as slow as a stalled network disk: `KRAAL_TEST_SLOW_KUBECONFIG_MS` long while a `<file>.slow`
/// marker sits next to it.
#[cfg(debug_assertions)]
fn slow_for_tests(path: &Path) {
    let Some(ms) = std::env::var("KRAAL_TEST_SLOW_KUBECONFIG_MS").ok().and_then(|v| v.parse().ok()) else { return };
    let mut marker = path.as_os_str().to_owned();
    marker.push(".slow");
    if Path::new(&marker).exists() {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

// ---- Terraform ---------------------------------------------------------------------------

/// Where the workspace of a Terraform command comes from: `TF_WORKSPACE`, else the directory
/// it works in (`-chdir`) with its data directory (`TF_DATA_DIR`). Nothing is read here (this
/// runs on the event loop): see [`terraform_dir_workspace`].
pub fn terraform_command_workspace(argv: &[String], cwd: &Path, env: &HashMap<String, String>) -> TfSeen {
    if let Some(ws) = env.get("TF_WORKSPACE").filter(|w| !w.is_empty()) {
        return TfSeen::Env(ws.clone());
    }
    let dir = match tool_args(argv, TERRAFORM).iter().find_map(|a| a.strip_prefix("-chdir=")) {
        Some(d) => cwd.join(crate::paths::expand_tilde(d)),
        None => cwd.to_path_buf(),
    };
    let data_dir = env.get("TF_DATA_DIR").filter(|d| !d.is_empty()).cloned();
    TfSeen::Dir { from: cwd.to_path_buf(), dir, data_dir }
}

/// The workspace selected in `dir` with data directory `data_dir` (`.terraform` by default),
/// `None` when it is not a Terraform directory (one never `terraform init`-ed).
pub fn terraform_dir_workspace(dir: &Path, data_dir: Option<&str>) -> Option<String> {
    let tf = dir.join(data_dir.unwrap_or(".terraform"));
    if !tf.is_dir() {
        return None;
    }
    #[cfg(debug_assertions)]
    slow_for_tests(&tf.join("environment"));
    // No environment file is the default workspace; one that cannot be read is unknown.
    let ws = match crate::paths::read_regular_text(&tf.join("environment"), 4096) {
        Ok(s) => s.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    Some(if ws.is_empty() { "default".into() } else { ws })
}

// ---- production --------------------------------------------------------------------------

/// Whether a name says production: one of `words` is a whole word of it, ignoring case and
/// trailing digits. `web-prod-1`, `prod-eks`, `db.prod.lan` and `prd01` are; `products-api` is not.
pub fn names_production(name: &str, words: &[String]) -> bool {
    name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_ascii_lowercase()).any(
        |t| {
            let bare = t.trim_end_matches(|c: char| c.is_ascii_digit());
            words.iter().any(|w| w.eq_ignore_ascii_case(&t) || (!bare.is_empty() && w.eq_ignore_ascii_case(bare)))
        },
    )
}

/// Whether a server is production: its `env` decides when set; otherwise its name, host or
/// groups.
pub fn server_production(s: &crate::servers::Server, words: &[String]) -> bool {
    if let Some(env) = &s.env {
        return names_production(env, words);
    }
    names_production(&s.name, words)
        || names_production(&s.host, words)
        || s.groups.iter().any(|g| names_production(g, words))
}

/// The local machine's name, to tell a remote shell's directory reports from local ones.
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn tool_detection() {
        assert!(runs_kube_tool(&argv("/usr/local/bin/kubectl get pods")));
        assert!(runs_kube_tool(&argv("watch -n 2 kubectl get pods")));
        assert!(runs_kube_tool(&argv("k9s")));
        assert!(!runs_kube_tool(&argv("vim kubectl-notes.md")));
        assert!(runs_aws_tool(&argv("/usr/bin/python3 /usr/bin/aws s3 ls")));
        assert!(runs_aws_tool(&argv("env AWS_PROFILE=x terraform plan")));
        assert!(!runs_aws_tool(&argv("zsh")));
        assert!(runs_kube_tool(&argv("/bin/sh /home/me/bin/kubectl get pods")));
        assert!(!runs_kube_tool(&argv("bash deploy.sh")));
    }

    #[test]
    fn kubeconfig() {
        let text = r#"apiVersion: v1
clusters:
- cluster:
    server: https://1.2.3.4
  name: prod
contexts:
- context:
    cluster: prod
    namespace: payments
    user: admin
  name: prod-eks
- name: "dev"
  context:
    cluster: dev
    user: me
current-context: prod-eks # set by kubectx
kind: Config
users:
- name: admin
  user:
    token: secret
"#;
        let f = parse_kubeconfig(text);
        assert_eq!(f.current.as_deref(), Some("prod-eks"));
        assert_eq!(f.namespaces.get("prod-eks"), Some(&Some("payments".to_string())));
        assert_eq!(f.namespaces.get("dev"), Some(&None));
        assert!(!f.namespaces.contains_key("admin"), "users are not contexts");
        let merged = merge_kubeconfigs(&[KubeFile::default(), f]);
        let c = merged.resolve(&KubeFlags::default()).unwrap();
        assert_eq!(c.label(false), "⎈ prod-eks/payments");
        let flags = kube_flags(&argv("kubectl --context=dev -n web get pods"));
        assert_eq!(merged.resolve(&flags).unwrap().label(false), "⎈ dev/web");
        let flags = kube_flags(&argv("kubectl --context dev get pods"));
        assert_eq!(merged.resolve(&flags).unwrap().label(false), "⎈ dev", "dev has no namespace");
        // A wrapper's options are its own; what follows `--` is not kubectl's.
        let flags = kube_flags(&argv("watch -n 2 kubectl --context dev -n prod get pods"));
        assert_eq!((flags.context.as_deref(), flags.namespace.as_deref()), (Some("dev"), Some("prod")));
        let flags = kube_flags(&argv("watch -n 2 kubectl get pods"));
        assert_eq!(flags.namespace, None);
        let flags = kube_flags(&argv("kubectl exec web -- sh -c kubectl -n other"));
        assert_eq!(flags.namespace, None);
        // A wrapper's option value is not the program, even named like it; wrappers chain.
        let flags = kube_flags(&argv("sudo -u kubectl watch -n 2 kubectl -n prod get pods"));
        assert_eq!(flags.namespace.as_deref(), Some("prod"));
        assert!(runs_kube_tool(&argv("env KUBECONFIG=/k nice -n 5 kubectl get pods")));
        assert!(!runs_kube_tool(&argv("sudo -u kubectl true")));
        assert!(!runs_kube_tool(&argv("sh -c kubectl")), "code for the shell names no program");
        // Each wrapper's own options: watch's directory is not the program, its -c is colour,
        // short options come together, timeout's duration comes before the program.
        let flags = kube_flags(&argv("watch --shotsdir /tmp/kubectl -n 2 kubectl -n prod get pods"));
        assert_eq!(flags.namespace.as_deref(), Some("prod"));
        let flags = kube_flags(&argv("watch -c kubectl --context prod get pods"));
        assert_eq!(flags.context.as_deref(), Some("prod"));
        let flags = kube_flags(&argv("watch -tn 2 kubectl -n prod get pods"));
        assert_eq!(flags.namespace.as_deref(), Some("prod"));
        let flags = kube_flags(&argv("timeout -s KILL 30 kubectl -n prod get pods"));
        assert_eq!(flags.namespace.as_deref(), Some("prod"));
        let flags = kube_flags(&argv("sudo -c staff KUBECONFIG=/k kubectl -n prod get pods"));
        assert_eq!(flags.namespace.as_deref(), Some("prod"));
        // `sudo -k` forgets the cached password and still runs the command.
        let flags = kube_flags(&argv("sudo -k kubectl --context prod get pods --watch"));
        assert_eq!(flags.context.as_deref(), Some("prod"));
        assert!(!runs_kube_tool(&argv("sudo -K")), "no command with -K");
        assert!(!runs_kube_tool(&argv("sudo -l kubectl")), "-l only lists what sudo allows");
        assert!(!runs_kube_tool(&argv("bash -lc kubectl")), "code for the shell, options together");
        assert!(!runs_kube_tool(&argv("env -S 'kubectl -n prod' get pods")));
        assert!(runs_kube_tool(&argv("python3 -W ignore /usr/bin/kubectl get pods")));
        assert_eq!(aws_profile_flag(&argv("sudo -u ops aws --profile prod s3 ls")).as_deref(), Some("prod"));
        assert_eq!(aws_profile_flag(&argv("aws --profile ops s3 ls")).as_deref(), Some("ops"));
        assert_eq!(absolute_kubeconfig("a.yaml:/etc/k", Some(Path::new("/w"))), "/w/a.yaml:/etc/k");
        assert_eq!(c.label(true), "k8s:prod-eks/payments");
        assert_eq!(short_context("arn:aws:eks:eu-west-1:123456789012:cluster/prod-eks"), "prod-eks");
        assert_eq!(short_context("gke_acme-123_europe-west1_staging"), "staging");
        assert!(parse_kubeconfig("current-context: \"\"\n").current.is_none());
        // What minikube writes: extensions with names of their own inside a context.
        let minikube = "contexts:\n- context:\n    cluster: minikube\n    extensions:\n    - extension:\n        provider: minikube.sigs.k8s.io\n      name: context_info\n    namespace: default\n    user: minikube\n  name: minikube\ncurrent-context: minikube\n";
        let f = parse_kubeconfig(minikube);
        assert_eq!(f.namespaces.len(), 1, "{f:?}");
        assert_eq!(f.namespaces.get("minikube"), Some(&Some("default".to_string())));
        // Flow style and JSON are kubeconfigs too.
        let f = parse_kubeconfig("{current-context: a, contexts: [{name: a, context: {namespace: x}}]}");
        assert_eq!((f.current.as_deref(), f.namespaces.get("a")), (Some("a"), Some(&Some("x".to_string()))));
        let f = parse_kubeconfig(r#"{"current-context": "b", "contexts": [{"name": "b", "context": {}}]}"#);
        assert_eq!((f.current.as_deref(), f.namespaces.get("b")), (Some("b"), Some(&None)));
        // A broken file is unknown, not "no context".
        let broken = parse_kubeconfig("contexts: [\n");
        assert!(broken.unreadable.is_some() && broken.current.is_none());
        assert!(merge_kubeconfigs(&[KubeFile::default(), broken]).unreadable.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn reads_told_out_of_order_never_bring_back_an_older_state() {
        let cache = KubeCache::default();
        let (first, second, third) = (cache.number(), cache.number(), cache.number());
        assert!(cache.told("k", first, true));
        assert_eq!(cache.known_since("k"), Some(first));
        // Read again, the same: the state still holds since the first read.
        assert!(cache.told("k", third, false));
        assert_eq!(cache.known_since("k"), Some(first));
        // A read that started before but ended after is not taken.
        assert!(!cache.told("k", second, true));
        assert_eq!(cache.known_since("k"), Some(first));
        // A change counts from the read that saw it.
        let fourth = cache.number();
        assert!(cache.told("k", fourth, true));
        assert_eq!(cache.known_since("k"), Some(fourth));
        cache.retain_told(|v| v != "k");
        assert_eq!(cache.known_since("k"), None);
    }

    #[test]
    fn a_kubeconfig_that_is_not_a_file_is_unknown_at_once() {
        let dir = std::env::temp_dir().join(format!("kraal-kubefifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("config");
        let _ = std::fs::remove_file(&fifo);
        let c = std::ffi::CString::new(fifo.display().to_string()).unwrap();
        // SAFETY: plain syscall on a path we own.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        let cache = KubeCache::default();
        let value = fifo.display().to_string();
        assert!(cache.begin(&value).is_some() && cache.begin(&value).is_none(), "one read of a value at a time");
        assert!(cache.begin("/elsewhere").is_some(), "others are read meanwhile");
        assert!(cache.reading(&value).is_some());
        let f = cache.merged(&value);
        cache.end(&value);
        assert!(cache.reading(&value).is_none());
        assert!(f.unreadable.is_some(), "{f:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn terraform() {
        let dir = std::env::temp_dir().join(format!("kraal-tf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(terraform_dir_workspace(&dir, None), None);
        std::fs::create_dir_all(dir.join(".terraform")).unwrap();
        assert_eq!(terraform_dir_workspace(&dir, None).as_deref(), Some("default"));
        std::fs::write(dir.join(".terraform/environment"), "prod\n").unwrap();
        assert_eq!(terraform_dir_workspace(&dir, None).as_deref(), Some("prod"));
        // What a command works on: -chdir, TF_DATA_DIR, TF_WORKSPACE.
        let infra = dir.join("infra");
        std::fs::create_dir_all(infra.join(".tfdata")).unwrap();
        std::fs::write(infra.join(".tfdata/environment"), "staging\n").unwrap();
        let mut env = HashMap::new();
        // Where the workspace comes from, then what it is there (read off the event loop).
        let ws = |a: &str, env: &HashMap<String, String>| match terraform_command_workspace(&argv(a), &dir, env) {
            TfSeen::Dir { from, dir: d, data_dir } => {
                assert_eq!(from, dir, "it holds where the command ran from");
                terraform_dir_workspace(&d, data_dir.as_deref())
            }
            TfSeen::Env(w) => Some(format!("env:{w}")),
        };
        assert_eq!(ws("terraform plan", &env).as_deref(), Some("prod"));
        assert_eq!(ws("terraform -chdir=infra plan", &env), None, "infra has no .terraform");
        env.insert("TF_DATA_DIR".to_string(), ".tfdata".to_string());
        assert_eq!(ws("terraform -chdir=infra plan", &env).as_deref(), Some("staging"));
        assert_eq!(ws("watch -n 5 terraform -chdir=infra plan", &env).as_deref(), Some("staging"));
        env.insert("TF_WORKSPACE".to_string(), "blue".to_string());
        assert_eq!(ws("terraform plan", &env).as_deref(), Some("env:blue"));
        // An environment file that is not a file is no workspace (and does not hang).
        std::fs::remove_file(dir.join(".terraform/environment")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", dir.join(".terraform/environment")).unwrap();
        assert_eq!(terraform_dir_workspace(&dir, None), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_names() {
        let words: Vec<String> = ["prod", "production", "prd"].map(String::from).to_vec();
        for yes in ["web-prod-1", "prod-eks", "db.prod.example.net", "PRODUCTION", "prd01", "eu_prod2"] {
            assert!(names_production(yes, &words), "{yes}");
        }
        for no in ["products-api", "staging", "preprod", "dev", ""] {
            assert!(!names_production(no, &words), "{no}");
        }
        let mut s = crate::servers::Server { name: "web1".into(), host: "web1.lan".into(), ..Default::default() };
        assert!(!server_production(&s, &words));
        s.groups = vec!["prod".into()];
        assert!(server_production(&s, &words));
        s.env = Some("staging".into());
        assert!(!server_production(&s, &words), "an explicit env wins");
        s.env = Some("prod".into());
        s.groups.clear();
        assert!(server_production(&s, &words));
    }
}
