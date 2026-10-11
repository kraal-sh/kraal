use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::*;

/// A directory for one test, and a fake ssh that runs the remote command here, with that
/// directory as the home directory: a server that is this machine.
struct Here {
    dir: PathBuf,
    ssh: String,
}

impl Here {
    fn new(name: &str) -> Here {
        let dir = std::env::temp_dir().join(format!("kraal-edit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let ssh = dir.join("fake-ssh");
        let home = crate::shell::quote(&dir.join("home").display().to_string());
        crate::testing::write_script(
            &ssh,
            &format!("#!/bin/sh\nfor a; do last=$a; done\ncd {home} && HOME={home} exec sh -c \"$last\"\n"),
        );
        Here { dir, ssh: ssh.display().to_string() }
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    fn open(&self, path: &str) -> Result<Opened> {
        open_with(&self.ssh, &["server".to_string()], path)
    }

    fn save(&self, opened: &Opened, expected: &str, content: &[u8]) -> Result<Saved> {
        save_with(&self.ssh, &["server".to_string()], &opened.file, expected, content)
    }

    /// What kraal left in `dir` besides the user's files.
    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".kraal-edit-"))
            .collect()
    }
}

impl Drop for Here {
    fn drop(&mut self) {
        // Directories a test made read-only can be emptied again.
        let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.dir).status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_new_version_replaces_the_file_with_its_mode() {
    let t = Here::new("round");
    let file = t.home().join("app.conf");
    std::fs::write(&file, "hello\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let before = std::fs::metadata(&file).unwrap().ino();

    let opened = t.open("~/app.conf").unwrap();
    assert_eq!(opened.content, b"hello\n");
    assert_eq!(opened.file.path(), std::fs::canonicalize(&file).unwrap().display().to_string());
    // Relative paths are in the home directory, and absolute ones are themselves.
    assert_eq!(t.open("app.conf").unwrap().file, opened.file);
    assert_eq!(t.open(&file.display().to_string()).unwrap().identity, opened.identity);

    let saved = t.save(&opened, &opened.identity, b"hello\nmore\n").unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\nmore\n");
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o640, "the mode is kept");
    assert_ne!(meta.ino(), before, "a new file was renamed over the old one");
    assert!(Here::leftovers(&t.home()).is_empty());
    // What the save says identifies the new version, as a new read does.
    let again = t.open("~/app.conf").unwrap();
    assert_eq!(saved, Saved::Written(again.identity.clone()));
    // An empty new version is a version too.
    assert!(matches!(t.save(&again, &again.identity, b"").unwrap(), Saved::Written(_)));
    assert_eq!(std::fs::read(&file).unwrap(), b"");
}

#[test]
fn a_file_changed_on_the_server_is_never_overwritten() {
    let t = Here::new("changed");
    let file = t.home().join("notes.txt");
    std::fs::write(&file, "first\n").unwrap();
    let opened = t.open("notes.txt").unwrap();
    // Someone else changes it meanwhile, keeping its size.
    std::fs::write(&file, "FIRST\n").unwrap();
    let Saved::Changed(now) = t.save(&opened, &opened.identity, b"mine\n").unwrap() else {
        panic!("a changed file was overwritten");
    };
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "FIRST\n");
    assert!(Here::leftovers(&t.home()).is_empty());
    assert_ne!(now, opened.identity);
    // Told what is there now, a save replaces that, and only that.
    assert!(matches!(t.save(&opened, &now, b"mine\n").unwrap(), Saved::Written(_)));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine\n");
    // Removed meanwhile: an error, nothing written.
    std::fs::remove_file(&file).unwrap();
    let err = t.save(&opened, &now, b"again\n").unwrap_err().to_string();
    assert!(err.contains("removed"), "{err}");
    assert!(!file.exists());
}

#[test]
fn odd_names_reach_the_shell_as_they_are() {
    let t = Here::new("names");
    let names = [
        "it's a \"file\" $(touch pwned) `touch pwned2` ;&|<>.conf",
        "-n",
        "--help",
        "new\nline",
        "  spaced  ",
        "*",
        "é 漢字.txt",
        "x\\y",
        "ends with newline\n",
    ];
    for name in names {
        let dir = t.home().join("d $HOME 'q'");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, format!("{name}\n")).unwrap();
        for path in [format!("~/d $HOME 'q'/{name}"), file.display().to_string()] {
            let opened = t.open(&path).unwrap_or_else(|e| panic!("{name:?}: {e:#}"));
            assert_eq!(opened.content, format!("{name}\n").as_bytes(), "{name:?}");
            assert_eq!(opened.file.name, name);
            assert_eq!(Path::new(&opened.file.dir), std::fs::canonicalize(&dir).unwrap());
            t.save(&opened, &opened.identity, b"edited\n").unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited\n", "{name:?}");
            std::fs::write(&file, format!("{name}\n")).unwrap();
        }
        assert!(Here::leftovers(&dir).is_empty());
    }
    let pwned: Vec<_> = walk(&t.dir).into_iter().filter(|p| p.to_string_lossy().contains("pwned")).collect();
    assert!(pwned.iter().all(|p| p.to_string_lossy().contains("$(touch pwned)")), "{pwned:?}");
    // Nor is the expected identity ever run.
    let opened = t.open("~/d $HOME 'q'/-n").unwrap();
    let err = t.save(&opened, "'; touch pwned3; '", b"x").unwrap();
    assert!(matches!(err, Saved::Changed(_)));
    assert!(!walk(&t.dir).iter().any(|p| p.ends_with("pwned3")));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() && !p.is_symlink() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn only_regular_files_are_edited() {
    let t = Here::new("kinds");
    let home = t.home();
    std::fs::create_dir_all(home.join("dir")).unwrap();
    std::os::unix::fs::symlink("dir", home.join("to-dir")).unwrap();
    std::os::unix::fs::symlink("missing", home.join("dangling")).unwrap();
    std::os::unix::fs::symlink("loop", home.join("loop")).unwrap();
    let fifo = std::ffi::CString::new(home.join("fifo").display().to_string()).unwrap();
    // SAFETY: a plain syscall on a path we own.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let refused = |path: &str, why: &str| {
        let err = format!("{:#}", t.open(path).unwrap_err());
        assert!(err.contains(why), "{path}: {err}");
    };
    refused("~/dir", "directory");
    refused("~/dir/", "directory");
    refused("~/to-dir", "directory");
    refused("~", "directory");
    refused("~/fifo", "not a regular file");
    refused("~/missing", "no such file");
    refused("~/dangling", "no such file");
    refused("~/loop", "too many levels");
    refused("~/nowhere/x", "");
    let big = home.join("big");
    std::fs::File::create(&big).unwrap().set_len(MAX_SIZE + 1).unwrap();
    refused("~/big", "larger than 16 MiB");
    assert!(t.open("").is_err());
}

#[test]
fn a_link_is_followed_to_the_file_it_names() {
    let t = Here::new("links");
    let home = t.home();
    std::fs::create_dir_all(home.join("sites-available")).unwrap();
    std::fs::create_dir_all(home.join("sites-enabled")).unwrap();
    std::fs::write(home.join("sites-available/default"), "server {}\n").unwrap();
    std::os::unix::fs::symlink("../sites-available/default", home.join("sites-enabled/default")).unwrap();
    std::os::unix::fs::symlink("sites-enabled/default", home.join("shortcut")).unwrap();
    let opened = t.open("~/shortcut").unwrap();
    assert_eq!(opened.file.name, "default");
    assert!(opened.file.dir.ends_with("/sites-available"), "{:?}", opened.file);
    t.save(&opened, &opened.identity, b"server { listen 80; }\n").unwrap();
    assert_eq!(std::fs::read_to_string(home.join("sites-available/default")).unwrap(), "server { listen 80; }\n");
    assert!(home.join("sites-enabled/default").is_symlink() && home.join("shortcut").is_symlink());
    // Replaced by a link since it was read: not written through.
    let real = home.join("sites-available/default");
    std::fs::remove_file(&real).unwrap();
    std::fs::write(home.join("elsewhere"), "precious\n").unwrap();
    std::os::unix::fs::symlink("../elsewhere", &real).unwrap();
    assert!(t.save(&opened, &opened.identity, b"x\n").is_err());
    assert_eq!(std::fs::read_to_string(home.join("elsewhere")).unwrap(), "precious\n");
}

#[test]
fn a_new_version_cut_short_changes_nothing() {
    let t = Here::new("short");
    let file = t.home().join("f");
    std::fs::write(&file, "old\n").unwrap();
    let opened = t.open("~/f").unwrap();
    // The server is told 10 bytes, and gets 4 (the connection dropped).
    let script = save_script(&opened.file, &opened.identity, 10);
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"new\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not arrive whole"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    assert!(Here::leftovers(&t.home()).is_empty());
}

#[test]
fn what_one_may_not_write_is_refused_before_editing() {
    // SAFETY: plain syscall.
    if unsafe { libc::geteuid() } == 0 {
        return; // root may write anything
    }
    let t = Here::new("perms");
    let home = t.home();
    std::fs::write(home.join("ro"), "x").unwrap();
    std::fs::set_permissions(home.join("ro"), std::fs::Permissions::from_mode(0o444)).unwrap();
    let err = format!("{:#}", t.open("~/ro").unwrap_err());
    assert!(err.contains("may not write") && err.contains("sudo"), "{err}");
    std::fs::create_dir_all(home.join("locked")).unwrap();
    std::fs::write(home.join("locked/f"), "x").unwrap();
    std::fs::set_permissions(home.join("locked"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let err = format!("{:#}", t.open("~/locked/f").unwrap_err());
    assert!(err.contains("its directory"), "{err}");
    std::fs::write(home.join("secret"), "x").unwrap();
    std::fs::set_permissions(home.join("secret"), std::fs::Permissions::from_mode(0o200)).unwrap();
    assert!(format!("{:#}", t.open("~/secret").unwrap_err()).contains("may not read"));
}

#[test]
fn answers_are_found_after_login_noise() {
    let mut out = b"Welcome to db\nKRAAL:EDIT\0/etc\0hosts\0abc  -\x005\0".to_vec();
    out.extend_from_slice(b"1\0\n23\0abc  -\0");
    let opened = parse_opened(&out).unwrap();
    assert_eq!(opened.file, RemoteFile { dir: "/etc".into(), name: "hosts".into() });
    assert_eq!(opened.content, b"1\0\n23", "the content is binary-safe");
    assert_eq!(opened.identity, "abc  -");
    assert_eq!(RemoteFile { dir: "/".into(), name: "x".into() }.path(), "/x");
    // Changed while it was read, or cut short: refused.
    let changed = b"KRAAL:EDIT\0/etc\0hosts\0abc\x002\0hi\0def\0";
    assert!(parse_opened(changed).unwrap_err().to_string().contains("changed while"));
    assert!(parse_opened(b"KRAAL:EDIT\0/etc\0hosts\0abc\x009\0hi\0abc\0").is_err());
    assert!(parse_opened(b"KRAAL:EDIT\0/etc\0\xff\0abc\x002\0hi\0abc\0").unwrap_err().to_string().contains("UTF-8"));
    assert!(parse_opened(b"nothing").is_err());
    assert_eq!(parse_saved(b"motd\nKRAAL:SAVED\0abc\0").unwrap(), Saved::Written("abc".into()));
    assert_eq!(parse_saved(b"KRAAL:CHANGED\0def\0").unwrap(), Saved::Changed("def".into()));
    assert!(parse_saved(b"").is_err());
}

#[test]
fn local_copies_are_private_and_kept_copies_never_replace() {
    let t = Here::new("local");
    let base = t.dir.join("edit");
    let file = local_copy(&base, "app.conf", b"secret").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"secret");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    let dir = file.parent().unwrap();
    assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode() & 0o777, 0o700);
    assert_ne!(local_copy(&base, "app.conf", b"other").unwrap().parent(), Some(dir));

    let downloads = t.dir.join("downloads");
    let first = keep(&downloads, "app.conf", b"one").unwrap();
    let second = keep(&downloads, "app.conf", b"two").unwrap();
    let third = keep(&downloads, ".bashrc", b"three").unwrap();
    assert_eq!(first, downloads.join("app.conf"));
    assert_eq!(second, downloads.join("app-1.conf"));
    assert_eq!(third, downloads.join(".bashrc"));
    assert_eq!(keep(&downloads, ".bashrc", b"four").unwrap(), downloads.join(".bashrc-1"));
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    assert_eq!(std::fs::metadata(&second).unwrap().permissions().mode() & 0o777, 0o600);
    // A link in the way is never written through.
    std::os::unix::fs::symlink(t.dir.join("target"), downloads.join("linked")).unwrap();
    assert_eq!(keep(&downloads, "linked", b"x").unwrap(), downloads.join("linked-1"));
    assert!(!t.dir.join("target").exists());
}

#[test]
fn a_save_is_seen_however_the_editor_writes() {
    let t = Here::new("stamp");
    let file = t.dir.join("f");
    std::fs::write(&file, "a").unwrap();
    let first = stamp(&file).unwrap();
    assert_eq!(stamp(&file), Some(first));
    // Written in place, the same size (later than the clock's granularity).
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(&file, "b").unwrap();
    let second = stamp(&file).unwrap();
    assert_ne!(second, first);
    // Replaced by a rename.
    std::fs::write(t.dir.join("g"), "b").unwrap();
    std::fs::rename(t.dir.join("g"), &file).unwrap();
    assert_ne!(stamp(&file).unwrap(), second);
    assert_eq!(stamp(&t.dir.join("gone")), None);
}
