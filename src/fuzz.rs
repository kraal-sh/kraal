//! Randomized tests of what kraal reads from outside: pane output, the user's terminal, files,
//! servers. A small generator, and a runner that names the seed of a failing case.
//!
//! `cargo test` runs a few cases of each target, from fixed seeds. To search longer, from a
//! random seed: `KRAAL_FUZZ_SECS=600 cargo test --release fuzz_` (each target for that long), or
//! `KRAAL_FUZZ_CASES=<n>`. A failure names its seed: `KRAAL_FUZZ_SEED=<seed> KRAAL_FUZZ_CASES=1`
//! runs that case again. A case running longer than `KRAAL_FUZZ_LIMIT` seconds (60) stops the
//! process, naming its seed; so does one taking more than `KRAAL_FUZZ_RSS_MB` of memory (Linux).
//! `KRAAL_FUZZ_KEEP_GOING` goes on after a failing case, `KRAAL_FUZZ_TRACE` says what each does.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A small, fast generator (splitmix64): the same seed gives the same case.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n` (0 when `n` is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.u64() % n as u64) as usize }
    }

    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// True once in `n` times.
    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// One of `items`, bytes.
    pub fn one_of<'a>(&mut self, items: &[&'a [u8]]) -> &'a [u8] {
        items[self.below(items.len())]
    }

    /// A size, small most of the time: up to `max`.
    pub fn size(&mut self, max: usize) -> usize {
        match self.below(10) {
            0..6 => self.below(max.min(16) + 1),
            6..9 => self.below(max.min(256) + 1),
            _ => self.below(max + 1),
        }
    }

    /// A number as programs write them, edge values often.
    pub fn number(&mut self) -> u64 {
        match self.below(12) {
            0 => 0,
            1 => 1,
            2..5 => self.below(10) as u64,
            5..8 => self.below(300) as u64,
            8 => *self.pick(&[127, 128, 255, 256, 1023, 1024, 4095, 4096, 4097, 32767, 32768, 65535, 65536]),
            9 => *self.pick(&[i32::MAX as u64, u32::MAX as u64, u32::MAX as u64 + 1, i64::MAX as u64, u64::MAX]),
            _ => self.u64() >> self.below(64),
        }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u64() as u8).collect()
    }

    /// `data` cut where reads might cut it.
    pub fn chunks<'a>(&mut self, data: &'a [u8]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let n = match self.below(4) {
                0 => 1,
                1 => self.range(1, 8),
                2 => self.range(1, 512),
                _ => data.len() - at,
            };
            let end = (at + n).min(data.len());
            out.push(&data[at..end]);
            at = end;
        }
        out
    }

    /// Change `data` the ways damaged or hostile input differs from good input: bits flipped,
    /// bytes changed, added, removed or repeated, pieces swapped, `tokens` put in, the end cut.
    pub fn mutate(&mut self, data: &mut Vec<u8>, tokens: &[&[u8]]) {
        for _ in 0..self.range(1, 4) {
            let len = data.len();
            match self.below(9) {
                0 if len > 0 => {
                    let i = self.below(len);
                    data[i] ^= 1 << self.below(8);
                }
                1 if len > 0 => {
                    let i = self.below(len);
                    data[i] =
                        *self.pick(&[0, 0xff, 0x7f, 0x80, 0x1b, b';', b'"', b'\\', b'0', b'9', b'-', b'\n', b'{']);
                }
                2 => {
                    let i = self.below(len + 1);
                    let n = self.size(64);
                    let added = self.bytes(n);
                    data.splice(i..i, added);
                }
                3 if len > 0 => {
                    let i = self.below(len);
                    let n = self.size(len - i).max(1);
                    data.drain(i..i + n);
                }
                4 if len > 0 => {
                    let i = self.below(len);
                    let n = self.size(len - i).max(1);
                    let piece = data[i..i + n].to_vec();
                    for _ in 0..self.range(1, 8) {
                        data.splice(i..i, piece.iter().copied());
                    }
                }
                5 if len > 1 => {
                    let (a, b) = (self.below(len), self.below(len));
                    let (a, b) = (a.min(b), a.max(b));
                    let n = self.size(16).min(b - a).min(len - b);
                    for k in 0..n {
                        data.swap(a + k, b + k);
                    }
                }
                6 | 7 if !tokens.is_empty() => {
                    let i = self.below(len + 1);
                    let token = self.pick(tokens).to_vec();
                    data.splice(i..i, token);
                }
                8 if len > 0 => data.truncate(self.below(len)),
                _ => {}
            }
        }
    }

    /// Text of any kind: ASCII, wide and combining characters, controls, invalid UTF-8.
    pub fn text(&mut self, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let n = self.size(max);
        while out.len() < n {
            match self.below(16) {
                0..8 => {
                    let k = self.range(1, 12);
                    out.extend((0..k).map(|_| self.range(0x20, 0x7e) as u8));
                }
                8 => out.extend_from_slice(
                    self.pick(&[
                        "日本",
                        "語",
                        "é",
                        "e\u{301}",
                        "\u{200d}",
                        "🦀",
                        "a\u{fe0f}",
                        "\u{10EEEE}\u{305}\u{30d}",
                    ])
                    .as_bytes(),
                ),
                9 => out.extend_from_slice(self.one_of(&[
                    &b"\r\n"[..],
                    b"\n",
                    b"\r",
                    b"\t",
                    b"\x08",
                    b"\x07",
                    b"\x0e",
                    b"\x0f",
                    b"\x0b",
                    b"\x0c",
                    b"\x00",
                    b"\x7f",
                ])),
                10 => out.push(self.range(0x80, 0xff) as u8),
                11 => out.extend_from_slice(self.one_of(&[
                    &b"\xc2\x9b"[..],
                    b"\xc2\x9c",
                    b"\xe6\x97",
                    b"\xf0\x9f",
                    b"\xed\xa0\x80",
                    b"\xc0\xaf",
                ])),
                12 => {
                    let c = char::from_u32(self.u64() as u32 % 0x11_0000).unwrap_or('?');
                    let mut b = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                }
                _ => out.extend_from_slice(b" "),
            }
        }
        out
    }
}

/// How many cases to run, for how long, from which seed.
struct Plan {
    cases: u64,
    until: Option<Instant>,
    seed: u64,
    limit: Duration,
    rss_mb: Option<u64>,
    /// Go on after a failing case (`KRAAL_FUZZ_KEEP_GOING`): a long search finds more than one.
    keep_going: bool,
}

fn env_number(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    let v = v.trim();
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

fn plan(name: &str, ci: u64) -> Plan {
    let secs = env_number("KRAAL_FUZZ_SECS");
    let cases = env_number("KRAAL_FUZZ_CASES");
    let searching = secs.is_some() || cases.is_some();
    let seed = env_number("KRAAL_FUZZ_SEED").unwrap_or_else(|| {
        // The same cases in every run of the tests; another start for a search.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in name.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        if searching {
            h ^= std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
            h ^= u64::from(std::process::id()) << 32;
        }
        h
    });
    Plan {
        cases: cases.unwrap_or(if secs.is_some() { u64::MAX } else { ci }),
        until: secs.map(|s| Instant::now() + Duration::from_secs(s)),
        seed,
        limit: Duration::from_secs(env_number("KRAAL_FUZZ_LIMIT").unwrap_or(60)),
        rss_mb: env_number("KRAAL_FUZZ_RSS_MB"),
        keep_going: std::env::var_os("KRAAL_FUZZ_KEEP_GOING").is_some(),
    }
}

/// Resident memory of this process, in MB.
#[cfg(target_os = "linux")]
fn rss_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some((pages * 4096) >> 20)
}

#[cfg(not(target_os = "linux"))]
fn rss_mb() -> Option<u64> {
    None
}

/// The case being run, for the watchdog.
struct Running {
    seed: AtomicU64,
    /// Milliseconds since `start` when the case started; `u64::MAX` between cases.
    since: AtomicU64,
    start: Instant,
    done: AtomicBool,
}

/// Run `case` on generators seeded one after the other (see the module's documentation), and
/// fail naming the seed of the first case that panics.
pub fn run(name: &str, ci: u64, case: impl Fn(&mut Rng)) {
    let plan = plan(name, ci);
    let running = Arc::new(Running {
        seed: AtomicU64::new(plan.seed),
        since: AtomicU64::new(u64::MAX),
        start: Instant::now(),
        done: AtomicBool::new(false),
    });
    let watched = running.clone();
    let (limit, rss_limit, target) = (plan.limit, plan.rss_mb, name.to_string());
    let watchdog = std::thread::spawn(move || {
        while !watched.done.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            let since = watched.since.load(Ordering::Relaxed);
            if since == u64::MAX {
                continue;
            }
            let seed = watched.seed.load(Ordering::Relaxed);
            let took = watched.start.elapsed().saturating_sub(Duration::from_millis(since));
            let rss = rss_limit.and_then(|max| rss_mb().filter(|mb| *mb > max));
            if took > limit || rss.is_some() {
                let why = match rss {
                    Some(mb) => format!("took {mb} MB"),
                    None => format!("ran {took:?}"),
                };
                eprintln!(
                    "fuzz {target}: the case of seed {seed:#x} {why} (KRAAL_FUZZ_SEED={seed:#x} KRAAL_FUZZ_CASES=1)"
                );
                std::process::abort();
            }
        }
    });
    let mut ran = 0u64;
    let mut failed = Vec::new();
    while ran < plan.cases && plan.until.is_none_or(|t| Instant::now() < t) {
        let seed = plan.seed.wrapping_add(ran);
        running.seed.store(seed, Ordering::Relaxed);
        running.since.store(running.start.elapsed().as_millis() as u64, Ordering::Relaxed);
        let result = catch_unwind(AssertUnwindSafe(|| case(&mut Rng::new(seed))));
        running.since.store(u64::MAX, Ordering::Relaxed);
        ran += 1;
        if result.is_err() {
            eprintln!("fuzz {name}: the case of seed {seed:#x} failed (KRAAL_FUZZ_SEED={seed:#x} KRAAL_FUZZ_CASES=1)");
            failed.push(seed);
            if !plan.keep_going {
                break;
            }
        }
    }
    running.done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    if plan.until.is_some() || plan.cases != ci {
        eprintln!("fuzz {name}: {ran} cases from seed {:#x}, {} failed", plan.seed, failed.len());
    }
    assert!(failed.is_empty(), "fuzz {name}: the cases of seeds {failed:#x?} failed");
}

/// Say what a case does, when `KRAAL_FUZZ_TRACE` is set: to read a failing case.
pub fn trace(what: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("KRAAL_FUZZ_TRACE").is_some()) {
        eprintln!("{}", what());
    }
}

/// Output a program may write to its terminal: text, controls, and escape sequences of every
/// kind, valid or not, with `images` kitty graphics commands and sixel images among them.
pub fn terminal_output(rng: &mut Rng, max: usize, images: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let n = rng.size(max);
    while out.len() < n {
        terminal_piece(rng, &mut out, images);
    }
    out
}

/// One piece of terminal output.
pub fn terminal_piece(rng: &mut Rng, out: &mut Vec<u8>, images: bool) {
    let terminator = |rng: &mut Rng| -> &'static [u8] {
        rng.one_of(&[&b"\x07"[..], b"\x1b\\", b"\x1b\\", b"\x07", b"\x18", b"\x1a", b"\x1bx", b"", b"\x9c"])
    };
    match rng.below(26) {
        0..6 => out.extend(rng.text(80)),
        6..9 => csi(rng, out),
        9 => out.extend_from_slice(rng.one_of(&[
            &b"\x1b[?1049h"[..],
            b"\x1b[?1049l",
            b"\x1b[?47h",
            b"\x1b[?1047l",
            b"\x1b[?2026h",
            b"\x1b[?2026l",
            b"\x1b[>1u",
            b"\x1b[>31u",
            b"\x1b[<u",
            b"\x1b[<9999u",
            b"\x1b[=5;2u",
            b"\x1b[?u",
            b"\x1bc",
            b"\x1b[!p",
            b"\x1b7",
            b"\x1b8",
            b"\x1b[s",
            b"\x1b[u",
            b"\x1b#8",
            b"\x1b[3J",
            b"\x1b[2J",
            b"\x1b[J",
            b"\x1b[H",
            b"\x1b[r",
            b"\x1b[2;5r",
            b"\x1b[5;2r",
            b"\x1b[?6h",
            b"\x1b[?7l",
            b"\x1b[?7h",
            b"\x1b[4h",
            b"\x1b[4l",
            b"\x1b[20h",
            b"\x1b(0",
            b"\x1b(B",
            b"\x1b)0",
            b"\x1bM",
            b"\x1bD",
            b"\x1bE",
            b"\x1bH",
            b"\x1b[22t",
            b"\x1b[23t",
            b"\x1b[6n",
            b"\x1b[c",
            b"\x1b[>c",
            b"\x1b[?1$p",
            b"\x1b[18t",
            b"\x1b[14t",
            b"\x1b[?1000h",
            b"\x1b[?1006h",
            b"\x1b[?1004h",
            b"\x1b[?2004h",
            b"\x1b[?1h\x1b=",
        ])),
        10..12 => {
            // A line or many: what fills the history.
            let lines = if rng.one_in(8) { rng.range(50, 600) } else { rng.range(1, 8) };
            for i in 0..lines {
                out.extend_from_slice(format!("line {i} {}\r\n", "x".repeat(rng.below(120))).as_bytes());
            }
        }
        12..15 => {
            out.extend_from_slice(b"\x1b]");
            out.extend(osc_body(rng));
            out.extend_from_slice(terminator(rng));
        }
        15 => {
            // Many keyboard modes pushed, or titles: the stacks have limits.
            let piece: &[u8] =
                rng.one_of(&[&b"\x1b[>1u"[..], b"\x1b[22t", b"\x1b[>u\x1b[?1049h", b"\x1b[>3u\x1b[?1049l"]);
            out.extend(piece.repeat(rng.range(1, 1200)));
        }
        16 => {
            // DCS strings that are no images.
            out.extend_from_slice(rng.one_of(&[
                &b"\x1bP$qm"[..],
                b"\x1bP>|",
                b"\x1bPtmux;\x1b\x1b[1m",
                b"\x1bP1$r",
                b"\x1bP+q544e",
            ]));
            out.extend(rng.text(20));
            out.extend_from_slice(terminator(rng));
        }
        17 => {
            // SOS, PM and APC strings.
            out.extend_from_slice(rng.one_of(&[&b"\x1bX"[..], b"\x1b^", b"\x1b_", b"\x1b_x"]));
            out.extend(rng.text(20));
            out.extend_from_slice(terminator(rng));
        }
        18 | 19 if images => kitty(rng, out),
        20 if images => sixel(rng, out),
        21 => {
            // Placeholder cells a program writes itself, in a colour that names its image.
            out.extend_from_slice(
                format!("\x1b[38;2;{};{};{}m", rng.below(256), rng.below(256), rng.below(256)).as_bytes(),
            );
            if rng.one_in(2) {
                out.extend_from_slice(format!("\x1b[58;5;{}m", rng.below(256)).as_bytes());
            }
            for _ in 0..rng.range(1, 6) {
                out.extend_from_slice("\u{10EEEE}".as_bytes());
                for _ in 0..rng.below(4) {
                    let mark = crate::graphics::DIACRITICS[rng.below(crate::graphics::DIACRITICS.len())];
                    out.extend_from_slice(mark.to_string().as_bytes());
                }
            }
        }
        22 => {
            // Shell integration: a whole command.
            let status = rng.number() as i64;
            out.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07");
            out.extend(rng.text(30));
            out.extend_from_slice(b"\r\n\x1b]133;C\x07");
            for _ in 0..rng.below(30) {
                out.extend(rng.text(40));
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(format!("\x1b]133;D;{status}\x07").as_bytes());
        }
        23 => {
            // A byte that may be cut anywhere.
            out.push(*rng.pick(&[0x1b, b'[', b']', b';', b'?', b'>', b'P', b'_', b'\\', 0x07, 0x18, 0x9c, 0x9b, 0x90]));
        }
        _ => {
            let n = rng.size(32);
            out.extend(rng.bytes(n));
        }
    }
}

fn csi(rng: &mut Rng, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[");
    if rng.one_in(3) {
        out.push(*rng.pick(b"?><=!"));
    }
    for i in 0..rng.size(20) {
        if i > 0 {
            out.push(if rng.one_in(6) { b':' } else { b';' });
        }
        if !rng.one_in(6) {
            out.extend_from_slice(rng.number().to_string().as_bytes());
        }
    }
    if rng.one_in(6) {
        out.push(*rng.pick(b" $\"'#*"));
    }
    let finals = b"@ABCDEFGHIJKLMPSTXZ`abcdefghilmnpqrstuxz{|}~";
    out.push(*rng.pick(finals));
}

fn osc_body(rng: &mut Rng) -> Vec<u8> {
    let long = |rng: &mut Rng| "x".repeat(rng.size(20_000));
    let body = match rng.below(20) {
        0 => format!("0;{}", String::from_utf8_lossy(&rng.text(200))),
        1 => format!("2;{}", long(rng)),
        2 => format!("4;{};rgb:{:x}/{:x}/{:x}", rng.number(), rng.below(0x10000), rng.below(256), rng.below(16)),
        3 => format!("4;{};?", rng.below(300)),
        4 => format!(
            "7;file://{}/{}",
            rng.pick(&["", "host", "localhost", "a.b"]),
            rng.pick(&["tmp", "a%20b", "%zz%", "%", "%4", "é/x"])
        ),
        5 => format!("8;id={};{}", rng.below(10), rng.pick(&["https://example.org/a", "", "file:///etc"])),
        6 => "8;;".to_string(),
        7 => format!("9;{}", String::from_utf8_lossy(&rng.text(400))),
        8 => {
            format!("777;notify;{};{}", String::from_utf8_lossy(&rng.text(40)), String::from_utf8_lossy(&rng.text(40)))
        }
        9 => format!(
            "{};{}",
            rng.pick(&["10", "11", "12", "17", "19", "104", "110", "111"]),
            rng.pick(&["?", "#ff0000", "rgb:1/2/3", "", "red"])
        ),
        10 => format!("52;{};{}", rng.pick(&["c", "p", "s", "", "cpq"]), rng.pick(&["aGk=", "?", "!!", "aGk", "=="])),
        11 => format!("52;c;{}", "QUJD".repeat(rng.size(50_000))),
        12 => format!("133;{}", rng.pick(&["A", "B", "C", "D", "D;1", "D;x", "D;99999999999", "A;cl=m", "Z", ""])),
        13 => format!(
            "7701;{}",
            rng.pick(&[
                "?",
                "context;k8s=a;ns=b;tf=c;aws=d",
                "context",
                "attach;gpu;main",
                "attach",
                "attach;;",
                "shows;3",
                "shows;x",
                "shows"
            ])
        ),
        14 => format!("1337;File=inline=1:{}", "QUJD".repeat(rng.size(100))),
        15 => format!("{};{}", rng.number(), String::from_utf8_lossy(&rng.text(40))),
        16 => format!("9;4;{};{}", rng.below(5), rng.below(200)),
        17 => format!("22;{}", rng.pick(&["pointer", "text", "?", ""])),
        _ => String::from_utf8_lossy(&rng.text(60)).into_owned(),
    };
    body.into_bytes()
}

/// A kitty graphics command (`ESC _ G … ESC \`), valid or not.
pub fn kitty(rng: &mut Rng, out: &mut Vec<u8>) {
    use base64::Engine;
    let b64 = |data: &[u8]| base64::engine::general_purpose::STANDARD.encode(data);
    let mut keys: Vec<String> = Vec::new();
    let (w, h) = (rng.range(1, 6) as u32, rng.range(1, 6) as u32);
    let mut payload = String::new();
    match rng.below(8) {
        0..3 => {
            let action = *rng.pick(&["t", "T", "q", "T"]);
            keys.push(format!("a={action}"));
            match rng.below(5) {
                0 => {
                    keys.push("f=100".into());
                    payload = b64(&tiny_png(w, h));
                }
                1 => {
                    keys.extend(["f=24".into(), format!("s={w}"), format!("v={h}")]);
                    payload = b64(&vec![200; (w * h * 3) as usize]);
                }
                2 => {
                    keys.extend(["f=32".into(), format!("s={w}"), format!("v={h}"), "o=z".into()]);
                    let raw = vec![7u8; (w * h * 4) as usize];
                    payload = b64(&miniz_oxide::deflate::compress_to_vec_zlib(&raw, 1));
                }
                3 => {
                    keys.extend([
                        format!("f={}", rng.number()),
                        format!("s={}", rng.number()),
                        format!("v={}", rng.number()),
                    ]);
                    let n = rng.size(64);
                    payload = b64(&rng.bytes(n));
                }
                _ => {
                    // A PNG damaged on the way.
                    keys.push("f=100".into());
                    let mut png = tiny_png(w, h);
                    rng.mutate(&mut png, &[]);
                    payload = b64(&png);
                }
            }
        }
        3 => keys.extend([
            "a=p".into(),
            format!("x={}", rng.number()),
            format!("y={}", rng.number()),
            format!("w={}", rng.number()),
            format!("h={}", rng.number()),
        ]),
        4 => keys.extend(["a=p".into(), "U=1".into(), format!("c={}", rng.number()), format!("r={}", rng.number())]),
        5 => keys.push(format!(
            "a=d,d={}",
            rng.pick(&["a", "A", "i", "I", "n", "N", "r", "R", "c", "C", "p", "x", "y", "z", "q"])
        )),
        6 => {
            keys.push("m=1".into());
            payload = "QUJD".repeat(rng.size(50));
        }
        _ => {}
    }
    for key in ["i", "I", "p", "q", "C", "m", "x", "y", "c", "r", "t", "U", "o"] {
        if rng.one_in(6) {
            keys.push(format!(
                "{key}={}",
                if key == "t" || key == "o" {
                    rng.pick(&["d", "f", "s", "z"]).to_string()
                } else {
                    rng.number().to_string()
                }
            ));
        }
    }
    if rng.one_in(3) {
        // An image id or a number, small so that commands meet.
        keys.push(format!("{}={}", rng.pick(&["i", "I"]), rng.range(1, 4)));
    }
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(keys.join(",").as_bytes());
    if !payload.is_empty() || rng.one_in(2) {
        out.push(b';');
        out.extend_from_slice(payload.as_bytes());
    }
    out.extend_from_slice(rng.one_of(&[&b"\x1b\\"[..], b"\x1b\\", b"\x1b\\", b"\x07", b"\x18", b""]));
}

/// A `w × h` PNG.
pub fn tiny_png(w: u32, h: u32) -> Vec<u8> {
    crate::graphics::encode_png(w, h, &vec![99; (w * h * 4) as usize]).unwrap()
}

/// A sixel image (`ESC P … q … ESC \`), valid or not.
pub fn sixel(rng: &mut Rng, out: &mut Vec<u8>) {
    out.extend_from_slice(rng.one_of(&[&b"\x1bPq"[..], b"\x1bP0;1;0q", b"\x1bP9q"]));
    for _ in 0..rng.size(40) {
        match rng.below(8) {
            0 => out.extend_from_slice(
                format!("\"{};{};{};{}", rng.below(3), rng.below(3), rng.number() % 3000, rng.number() % 3000)
                    .as_bytes(),
            ),
            1 => out.extend_from_slice(
                format!("#{};{};{};{};{}", rng.number(), rng.below(3), rng.number(), rng.number(), rng.number())
                    .as_bytes(),
            ),
            2 => out.extend_from_slice(format!("#{}", rng.below(20)).as_bytes()),
            3 => out.extend_from_slice(
                format!("!{}{}", rng.number() % 5000, (0x3f + rng.below(64)) as u8 as char).as_bytes(),
            ),
            4 => out.push(*rng.pick(b"$-")),
            _ => {
                for _ in 0..rng.range(1, 20) {
                    out.push((0x3f + rng.below(64)) as u8);
                }
            }
        }
    }
    out.extend_from_slice(rng.one_of(&[&b"\x1b\\"[..], b"\x1b\\", b"\x18", b"\x07", b""]));
}

/// The files kraal reads (the config, the server inventory, the accounts, runbooks, a
/// worktree's setup, the paste buffers, a saved session, kubeconfigs), as written or damaged any
/// way: read without a panic, and what is read used as kraal uses it.
#[cfg(test)]
mod files {
    use super::{Rng, run};

    const TOKENS: &[&[u8]] = &[
        b"\n",
        b"=",
        b"[",
        b"]",
        b"\"",
        b"{",
        b"}",
        b",",
        b"#",
        b"[[servers]]",
        b"\nname = \"x\"",
        b"-1",
        b"99999999999999999999",
        b"true",
        b"'''",
        b"\\u0000",
        b"null",
        b":",
        b"\n  - ",
    ];

    fn damaged(rng: &mut Rng, sample: &str) -> String {
        let mut text = sample.as_bytes().to_vec();
        for _ in 0..rng.range(0, 3) {
            rng.mutate(&mut text, TOKENS);
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    /// A file holding `text`, named with `extension`.
    fn temp_file(text: &str, extension: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("kraal-fuzz-{}-{n}.{extension}", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn fuzz_files_config() {
        run("files_config", 100, |rng| {
            let user = damaged(
                rng,
                "scrollback = 500\nshell = \"zsh\"\n[graphics]\nquota_mb = 0\n[worktrees]\ncopy = [\".env\"]\nsetup = [\"npm ci\"]\n",
            );
            let _ = crate::config::Config::from_user_toml(&user);
            let _ = crate::config::Config::from_user_toml(&damaged(rng, crate::config::DEFAULT_CONFIG));
        });
    }

    #[test]
    fn fuzz_files_servers() {
        run("files_servers", 100, |rng| {
            let text = damaged(
                rng,
                "[[servers]]\nname = \"web-1\"\nhost = \"10.0.0.1\"\nuser = \"deploy\"\nport = 2222\njump = \"bastion\"\ngroup = \"prod\"\n\n[[servers]]\nname = \"bastion\"\nhost = \"b.example.net\"\nidentities = [\"~/.ssh/id\"]\n",
            );
            let path = temp_file(&text, "toml");
            if let Ok(inv) = crate::servers::Inventory::load_from(&path) {
                let known = crate::servers::known(&inv, &[], &["h".into()]);
                for k in &known {
                    let _ = crate::servers::ssh_args(&k.server, &known, rng.one_in(2));
                    let _ = k.server.destination();
                }
                let _ = crate::servers::groups(&known);
                let _ = crate::servers::resolve(&known, &damaged(rng, "deploy@web-1:2222"));
            }
            let _ = std::fs::remove_file(&path);
            let words: Vec<String> =
                damaged(rng, "-p 2222 -J bastion -i key deploy@web-1 ls -l").split(' ').map(String::from).collect();
            let _ = crate::servers::parse_ssh_args(&words);
            let _ = crate::servers::ssh_runs_command(&words);
            let _ = crate::servers::parse_destination(&damaged(rng, "user@[::1]:22"));
        });
    }

    /// Whatever a file looks like, a change kraal writes into it is read back as kraal meant
    /// it.
    #[test]
    fn fuzz_files_rewritten() {
        use crate::servers::{Inventory, Server};
        run("files_rewritten", 100, |rng| {
            let text = damaged(
                rng,
                "# Ours.\n\n[[server]]\nname = \"web-1\" # the first\nhost = \"192.0.2.1\"\nuser = \"deploy\"\ncolor = \"red\"\ngroups = [\"web\", \"prod\"]\n\n# The way in.\n[[server]]\nhost = \"b.example.net\"\nname = \"bastion\"\nmore_identities = [\n    \"~/.ssh/id\", # mine\n]\n",
            );
            let path = temp_file(&text, "toml");
            if Inventory::load_from(&path).is_ok() {
                let change = rng.below(4);
                let wanted = Inventory::update_at(&path, |inv| {
                    match change {
                        0 => drop(inv.add(Server { host: "new.example.net".into(), ..Default::default() })?),
                        1 => drop(inv.servers.pop()),
                        2 => inv.servers.reverse(),
                        _ => {
                            if let Some(first) = inv.servers.first_mut() {
                                first.agents = Some("main".into());
                                first.groups.retain(|group| group != "web");
                            }
                        }
                    }
                    Ok(inv.servers.clone())
                });
                let written = std::fs::read_to_string(&path).unwrap();
                let wanted = wanted.unwrap_or_else(|e| panic!("{e:#}: {text:?}"));
                let read = Inventory::load_from(&path).map(|inv| inv.servers);
                assert_eq!(read.ok(), Some(wanted), "{text:?} became {written:?}");
            }
            let _ = std::fs::remove_file(&path);
            let lock = format!(".{}.lock", path.file_name().unwrap().to_string_lossy());
            let _ = std::fs::remove_file(path.with_file_name(lock));
        });
    }

    #[test]
    fn fuzz_files_accounts() {
        run("files_accounts", 100, |rng| {
            let text = damaged(
                rng,
                "[[claude]]\nname = \"work\"\nhome = \"~/.claude-work\"\n\n[[codex]]\nname = \"me\"\n\n[[rules]]\nworkspace = \"api*\"\nagent = \"claude\"\naccounts = [\"work\"]\n",
            );
            let path = temp_file(&text, "toml");
            if let Ok(accounts) = crate::accounts::Accounts::load_from(&path) {
                let _ = format!("{accounts:?}");
            }
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn fuzz_files_runbooks() {
        run("files_runbooks", 100, |rng| {
            let steps = damaged(
                rng,
                "description = \"x\"\nshell = \"bash\"\ntimeout = \"10m\"\nsteps = [\"echo $NAME\", \"ls\"]\n[[params]]\nname = \"NAME\"\nsecret = true\n",
            );
            let md =
                damaged(rng, "# Deploy\n\nSome text.\n\n```bash\nmake deploy\n```\n\n```console\n$ ls\nfile\n```\n");
            for path in [temp_file(&steps, "toml"), temp_file(&md, "md")] {
                if let Ok(runbook) = crate::runbooks::load(&path, crate::runbooks::Source::User) {
                    let _ = runbook.description();
                    if let crate::runbooks::Body::Steps(s) = &runbook.body {
                        let _ = s.timeout();
                        let _ = crate::runbooks::script(&s.steps, s.stop_on_failure);
                    }
                }
                let _ = std::fs::remove_file(&path);
            }
            let _ = crate::worktree_setup::Setup::parse(&damaged(
                rng,
                "copy = [\".env\", \"config/*.local\"]\nsetup = [\"npm ci\"]\n",
            ));
        });
    }

    #[test]
    fn fuzz_files_buffers() {
        run("files_buffers", 100, |rng| {
            let mut buffers = crate::buffers::Buffers::new(rng.range(0, 50), rng.range(0, 1 << 20));
            let saved = damaged(
                rng,
                "{\"version\":1,\"buffers\":[{\"name\":\"a\",\"text\":\"hello\",\"id\":3},{\"name\":null,\"text\":\"x\"}]}",
            );
            let _ = buffers.load_json(saved.as_bytes());
            let _ = buffers.to_json();
        });
    }

    #[test]
    fn fuzz_files_session() {
        run("files_session", 100, |rng| {
            let snap = damaged(
                rng,
                r#"{"version":1,"revision":1,"workspaces":[{"name":"w","definition":null,"cwd":"/tmp","active":3,"tabs":[{"name":"t","root":{"Split":{"axis":"Row","children":[[{"Pane":{"name":null,"spec":{"command":["ssh","h"]},"agent":"claude","agent_session":"a b","scrollback":"1-2-3.ans"}},1.0],[{"Pane":{"name":"x","spec":{},"agent":null,"agent_session":null,"scrollback":"../x"}},-2.0]],"stacked":true,"open":7}},"floating":[],"focus":9,"show_floating":true,"zoomed":true,"sync":false}]}]}"#,
            );
            if let Ok(snap) = serde_json::from_str::<crate::persist::SessionSnapshot>(&snap) {
                let _ = crate::persist::scrollback_files(&snap);
                for ws in &snap.workspaces {
                    for t in &ws.tabs {
                        for (p, rect) in &t.floating {
                            let _ = rect.to_rect(ratatui::layout::Rect::new(
                                0,
                                0,
                                rng.below(300) as u16,
                                rng.below(100) as u16,
                            ));
                            let _ = crate::persist::resume_command(
                                p.agent.as_deref().unwrap_or(""),
                                p.agent_session.as_deref().unwrap_or(""),
                            );
                        }
                    }
                }
            }
            let contexts = damaged(rng, "k8s=prod;ns=web;tf=;aws=ops");
            let _ = crate::context::RemoteContexts::parse(&contexts);
            let kube = damaged(
                rng,
                "apiVersion: v1\ncurrent-context: prod\ncontexts:\n- name: prod\n  context:\n    cluster: c\n    namespace: web\n",
            );
            let _ = crate::context::parse_kubeconfig(&kube);
        });
    }
}
