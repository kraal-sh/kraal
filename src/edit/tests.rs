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
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let ssh = dir.join("fake-ssh");
        let home = crate::shell::quote(&dir.join("home").display().to_string());
        let bin = crate::shell::quote(&dir.join("bin").display().to_string());
        crate::testing::write_script(
            &ssh,
            &format!(
                "#!/bin/sh\nfor a; do last=$a; done\ncd {home} && HOME={home} PATH={bin}:$PATH exec sh -c \"$last\"\n"
            ),
        );
        Here { dir, ssh: ssh.display().to_string() }
    }

    /// The same, with a `sudo` there that takes the password `pass word` on its input (or none
    /// once `~/.nopasswd` exists) and runs the command as this user.
    fn with_sudo(name: &str) -> Here {
        let t = Here::new(name);
        crate::testing::write_script(
            &t.dir.join("bin/sudo"),
            "#!/bin/sh\nn=\nwhile [ $# -gt 0 ]; do case $1 in -n) n=1 ;; -p) shift ;; --) shift; break ;; -*) ;; *) break ;; esac; shift; done\n\
             if [ -n \"$n\" ]; then [ -e \"$HOME/.nopasswd\" ] || { echo 'sudo: a password is required' >&2; exit 1; }; exec \"$@\"; fi\n\
             IFS= read -r pw\nif [ \"$pw\" != 'pass word' ]; then echo 'Sorry, try again.' >&2; exit 1; fi\nexec \"$@\"\n",
        );
        t
    }

    fn session(&self, path: &str) -> Result<Session> {
        Session::start_with(&self.ssh, &["server".to_string()], path)
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
    let refused = t.open("~/ro").unwrap_err();
    let err = format!("{refused:#}");
    assert!(err.contains("may not write") && err.contains("kraal edit --sudo"), "{err}");
    assert!(refused.downcast_ref::<NotPermitted>().is_some(), "as root it could be edited");
    std::fs::create_dir_all(home.join("locked")).unwrap();
    std::fs::write(home.join("locked/f"), "x").unwrap();
    std::fs::set_permissions(home.join("locked"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let refused = t.open("~/locked/f").unwrap_err();
    assert!(format!("{refused:#}").contains("its directory"), "{refused:#}");
    assert!(refused.downcast_ref::<NotPermitted>().is_some());
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
    let (dir, file) = local_copy(&base, "app.conf", b"secret").unwrap();
    assert_eq!(file.parent(), Some(dir.as_path()));
    assert_eq!(std::fs::read(&file).unwrap(), b"secret");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    let dir = dir.as_path();
    assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode() & 0o777, 0o700);
    assert_ne!(local_copy(&base, "app.conf", b"other").unwrap().0, dir);

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

/// A server answering with a path for a name (or `..`) cannot make kraal write, or later remove,
/// anything outside the edit's private directory.
#[test]
fn a_name_from_the_server_never_leaves_the_private_directory() {
    let answer = |dir: &str, name: &str| {
        let mut out = format!("KRAAL:EDIT\0{dir}\0{name}\0abc\x002\0").into_bytes();
        out.extend_from_slice(b"hi\0abc\0");
        parse_opened(&out)
    };
    assert_eq!(answer("/etc", "hosts").unwrap().file.name, "hosts");
    for (dir, name) in [
        ("/etc", "/home/alice/Documents/new.txt"),
        ("/etc", "../x"),
        ("/etc", ".."),
        ("/etc", "."),
        ("/etc", "a/b"),
        ("etc", "hosts"),
        ("", "x"),
    ] {
        assert!(answer(dir, name).is_err(), "{dir:?} {name:?}");
    }
    // Nor do the local copies take such a name.
    let t = Here::new("names-local");
    let base = t.dir.join("edit");
    let outside = t.dir.join("outside.txt");
    for name in [outside.display().to_string(), "../outside.txt".into(), "..".into(), "".into()] {
        assert!(local_copy(&base, &name, b"x").is_err(), "{name:?}");
        assert!(keep(&t.dir.join("downloads"), &name, b"x").is_err(), "{name:?}");
    }
    assert!(!outside.exists());
}

/// Two saves of the same version (two kraal editors, or one upload still on its way when
/// another lands): the second one finds the file changed and writes nothing.
#[test]
fn two_saves_of_the_same_version_never_both_win() {
    let t = Here::new("race");
    let file = t.home().join("shared.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let opened = t.open("~/shared.txt").unwrap();
    // A slow upload: it has started, and passed its first check.
    let mut slow = std::process::Command::new("sh")
        .arg("-c")
        .arg(save_script(&opened.file, &opened.identity, 6))
        .current_dir(t.home())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = slow.stdin.take().unwrap();
    input.write_all(b"sl").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(matches!(t.save(&opened, &opened.identity, b"fast\n").unwrap(), Saved::Written(_)));
    input.write_all(b"ow\n\n").unwrap();
    drop(input);
    let out = slow.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(matches!(parse_saved(&out.stdout).unwrap(), Saved::Changed(_)), "the slow save overwrote the other one");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fast\n");
    assert!(Here::leftovers(&t.home()).is_empty());
}

/// A save waits while another kraal holds the file's lock, then goes on.
#[test]
fn a_save_waits_for_the_lock_of_another() {
    let t = Here::new("lock");
    let file = t.home().join("f.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let opened = t.open("~/f.txt").unwrap();
    let sum = std::process::Command::new("sh").arg("-c").arg("printf %s f.txt | cksum").output().unwrap();
    let sum = String::from_utf8(sum.stdout).unwrap();
    let lock = t.home().join(format!(".kraal-edit-lock-{}", sum.split_whitespace().next().unwrap()));
    std::fs::create_dir(&lock).unwrap();
    let held = lock.clone();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        std::fs::remove_dir(&held).unwrap();
    });
    let start = std::time::Instant::now();
    assert!(matches!(t.save(&opened, &opened.identity, b"v2\n").unwrap(), Saved::Written(_)));
    assert!(start.elapsed() >= Duration::from_millis(500), "it did not wait");
    release.join().unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2\n");
    assert!(Here::leftovers(&t.home()).is_empty());
}

/// The server's version is written over the editor's copy only while the copy is as it was when
/// the user chose it: a save made since is left alone.
#[test]
fn the_servers_version_never_replaces_a_copy_saved_meanwhile() {
    let t = Here::new("replace");
    let (dir, local) = local_copy(&t.dir.join("edit"), "f.txt", b"mine\n").unwrap();
    let seen = stamp(&local);
    let now = replace_copy(&local, seen, b"theirs\n").unwrap();
    assert!(now.is_some());
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "theirs\n");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&local, "mine again\n").unwrap();
    assert_eq!(replace_copy(&local, now, b"theirs 2\n").unwrap(), None);
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "mine again\n");
    let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec!["f.txt"]);
    // Whatever the file's name.
    let (_, local) = local_copy(&t.dir.join("edit"), ".kraal-new", b"mine\n").unwrap();
    assert!(replace_copy(&local, stamp(&local), b"theirs\n").unwrap().is_some());
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "theirs\n");
}

/// Through sudo: the password is handed over once, wiped, and every read and save of the edit
/// then goes through the same root session, with the same checks and lock.
#[test]
fn as_root_through_sudo_the_password_is_given_once() {
    let t = Here::with_sudo("sudo");
    let etc = t.home().join("etc");
    std::fs::create_dir_all(&etc).unwrap();
    let file = etc.join("app.conf");
    std::fs::write(&file, "a\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();

    // A home reached through a symbolic link (as /var is on macOS): the path is in it as the
    // user knows it, its links not resolved.
    let linked = t.dir.join("linked-home");
    std::os::unix::fs::symlink(t.home(), &linked).unwrap();
    let via_link = t.dir.join("fake-ssh-linked");
    let (home, bin) = (linked.display().to_string(), t.dir.join("bin").display().to_string());
    crate::testing::write_script(
        &via_link,
        &format!(
            "#!/bin/sh\nfor a; do last=$a; done\ncd {h} && HOME={h} PATH={b}:$PATH exec sh -c \"$last\"\n",
            h = crate::shell::quote(&home),
            b = crate::shell::quote(&bin)
        ),
    );
    let s = Session::start_with(&via_link.display().to_string(), &["server".to_string()], "etc/app.conf").unwrap();
    assert_eq!(s.path, format!("{home}/etc/app.conf"));
    drop(s);

    let mut s = t.session("etc/app.conf").unwrap();
    assert!(s.needs_password);
    assert_eq!(s.path, file.display().to_string(), "made absolute in the user's home there");
    assert!(s.open(&s.path.clone()).is_err(), "nothing before the password");
    let mut password = b"pass word".to_vec();
    s.authenticate(&mut password).unwrap();
    assert!(password.iter().all(|&b| b == 0), "the password is wiped");
    let path = s.path.clone();
    let opened = s.open(&path).unwrap();
    assert_eq!(opened.content, b"a\n");
    // What could pass for the end of an answer is content like any other.
    let content = b"KRAAL:END:\0KRAAL:SAVED\0\xff\nlast".to_vec();
    let saved = s.save(&opened.file, &opened.identity, &content).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), content);
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777, 0o640, "the mode is kept");
    let again = s.open(&path).unwrap();
    assert_eq!(again.content, content);
    assert_eq!(saved, Saved::Written(again.identity.clone()));
    assert!(matches!(s.save(&opened.file, &opened.identity, b"stale").unwrap(), Saved::Changed(_)));
    assert_eq!(std::fs::read(&file).unwrap(), content);
    // An error is said, and the session goes on.
    assert!(s.open("/nowhere/x").is_err());
    assert!(matches!(s.save(&again.file, &again.identity, b"").unwrap(), Saved::Written(_)));
    assert_eq!(std::fs::read(&file).unwrap(), b"");
    assert!(Here::leftovers(&etc).is_empty());
    drop(s);

    // A wrong password: refused.
    let mut s = t.session("~/etc/app.conf").unwrap();
    let err = s.authenticate(&mut b"wrong".to_vec()).unwrap_err().to_string();
    assert!(err.contains("sudo refused"), "{err}");
    // No password needed there: straight to work.
    std::fs::write(t.home().join(".nopasswd"), "").unwrap();
    let mut s = t.session("~/etc/app.conf").unwrap();
    assert!(!s.needs_password);
    assert_eq!(s.open(&file.display().to_string()).unwrap().content, b"");
}

/// The pane asking for the password hands it over through a FIFO, without a file on disk, and
/// says when it was not given.
#[test]
fn the_password_goes_from_its_pane_to_kraal() {
    let t = Here::new("fifo");
    let ask = |typed: &'static [u8]| {
        let dir = private_dir(&t.dir.join("edit")).unwrap();
        let fifo = password_fifo(&dir).unwrap();
        let command = password_command(&fifo, "Password: ");
        let reader = std::thread::spawn(move || read_password(&fifo, Duration::from_secs(10)));
        let mut pane = std::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        pane.stdin.take().unwrap().write_all(typed).unwrap();
        let out = pane.wait_with_output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("Password: "));
        reader.join().unwrap().unwrap()
    };
    assert_eq!(ask(b"pass word\n"), Some(b"pass word".to_vec()));
    assert_eq!(ask(b""), None, "the input ended: not given");
}

/// The editor is the user's: `$VISUAL`, else `$EDITOR`, else vi, each with its own options.
#[test]
fn the_editor_is_visual_then_editor_then_vi() {
    let t = Here::new("editor");
    crate::testing::write_script(&t.dir.join("bin/vi"), "#!/bin/sh\nprintf 'vi<%s>' \"$@\"\n");
    let argv = editor_argv(Path::new("/a dir/f"), Some(12));
    let path = format!("{}:/usr/bin:/bin", t.dir.join("bin").display());
    let run = |visual: Option<&str>, editor: Option<&str>| {
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..]).env("PATH", &path).env_remove("VISUAL").env_remove("EDITOR");
        c.envs(visual.map(|v| ("VISUAL", v))).envs(editor.map(|e| ("EDITOR", e)));
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    };
    assert_eq!(run(Some("printf V<%s>"), Some("printf E<%s>")), "V<+12>V</a dir/f>");
    assert_eq!(run(Some(""), Some("printf E<%s>")), "E<+12>E</a dir/f>", "an empty VISUAL is none");
    assert_eq!(run(None, None), "vi<+12>vi</a dir/f>");
}
