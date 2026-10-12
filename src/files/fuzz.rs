//! Randomized tests of what a server's listing says (see `crate::fuzz`): kraal reads any answer
//! without panicking, valid or not.

use super::listing::*;
use crate::fuzz::{self, Rng};

/// A listing as the remote script prints it, from GNU find (`F`) or the shell fallback (`S`).
fn listing(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    if !rng.one_in(10) {
        out.extend_from_slice(MARKER);
    }
    out.extend_from_slice(rng.pick(&["/srv/dir", "/", "", "/a\tb"]).as_bytes());
    out.push(0);
    let find = rng.one_in(2);
    out.extend_from_slice(if find { b"F" } else { b"S" });
    out.push(0);
    for _ in 0..rng.size(30) {
        let kind = *rng.pick(&["fd", "ff", "ld", "lf", "lN", "d", "", "x"]);
        let size = if rng.one_in(3) {
            rng.number().to_string()
        } else {
            rng.pick(&["4096", "", "-1", "8,1", "1e9"]).to_string()
        };
        let date = if find {
            format!("{}.{}", rng.number() as i64, rng.below(10))
        } else {
            let month = *rng.pick(&["Jan", "Feb", "Dec", "Foo", ""]);
            let time = match rng.below(4) {
                0 => format!("{}:{}", rng.number(), rng.number()),
                1 => rng.number().to_string(),
                2 => format!("-{}", rng.number()),
                _ => format!("@{}", rng.number() as i64),
            };
            if time.starts_with('@') { time } else { format!("{month} {} {time}", rng.number() % 40) }
        };
        let name = String::from_utf8_lossy(&rng.text(20)).into_owned();
        out.extend_from_slice(format!("{kind}\t{size}\t{date}\t{name}").as_bytes());
        out.push(0);
    }
    if rng.one_in(2) {
        rng.mutate(&mut out, &[b"\0", b"\t", b":", b"@", b"-", MARKER]);
    }
    out
}

#[test]
fn fuzz_files_listing() {
    fuzz::run("files_listing", 300, |rng| {
        let out = listing(rng);
        // This machine's clock: some time before the year 3000.
        let now = rng.below(32_503_680_000) as i64;
        if let Ok((dir, entries)) = parse_listing(&out, now) {
            assert!(!dir.is_empty());
            assert!(entries.iter().all(|e| !e.name.is_empty() && e.name != "." && e.name != ".."));
        }
    });
}
