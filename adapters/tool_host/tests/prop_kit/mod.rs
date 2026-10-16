//! A small seeded generator and a case loop for property-style and fuzz-style tests (card OA13-01, tasks QA-06 and QA-07). Standard library only: the project adds no test crate
//! (no `proptest`, no `quickcheck`, no `libfuzzer`), so this is the whole of it.
//!
//! One file, three users. It is read as a module (`mod prop_kit;`) by the test files of `aqua-tool-host`, and by `#[path = "../../tool_host/tests/prop_kit/mod.rs"]` by those of
//! `aqua-native-tools` and `aqua-mcp-adapters`, and `aqua-core` takes it into `aqua_core::test_support::prop` the same way. It lives under `tests/` and not in `src/` so that no
//! release build holds it, and in a folder with a `mod.rs` so that cargo does not take it for a test of its own.
//!
//! **Deterministic by default.** Every property has a fixed number of cases and the seed is a constant, so a run today is the run of yesterday and a failure is a failure of the
//! code, never of the dice. **A failure can be replayed**: the case loop prints (and puts in the panic message) the seed, the number of the case and the line to replay it with.
//!
//! * `AQUA_PROP_SEED=<number or 0xhex>`: another seed for every property (a bigger hunt: loop over seeds in a shell).
//! * `AQUA_PROP_CASES=<n>`: `n` cases for every property instead of the number the test names (a deep run: `AQUA_PROP_CASES=20000`).
//! * `AQUA_PROP_CASE=<i>`: only case `i` (what the failure message says to set, with the seed).
//!
//! A property that is slow on purpose (it starts a program or opens a database for every case) names a small number of cases and says so; `AQUA_PROP_CASES` still replaces it.
#![allow(dead_code)]

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

/// The seed of a run that names none.
pub const DEFAULT_SEED: u64 = 0x0A9A_2026_1010_0001;

fn parse_number(text: &str) -> Option<u64> {
    let t = text.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

/// The seed of this run: `AQUA_PROP_SEED`, or [`DEFAULT_SEED`].
pub fn base_seed() -> u64 {
    std::env::var("AQUA_PROP_SEED").ok().and_then(|v| parse_number(&v)).unwrap_or(DEFAULT_SEED)
}

/// How many cases a property runs: `AQUA_PROP_CASES`, or what the test named.
pub fn cases_or(default: u32) -> u32 {
    std::env::var("AQUA_PROP_CASES").ok().and_then(|v| parse_number(&v)).map(|n| n.min(u32::MAX as u64) as u32).unwrap_or(default)
}

/// Only this case (`AQUA_PROP_CASE`), to replay a failure.
pub fn only_case() -> Option<u32> {
    std::env::var("AQUA_PROP_CASE").ok().and_then(|v| parse_number(&v)).map(|n| n as u32)
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fnv(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// A seeded generator (SplitMix64: small, fast, well mixed, and the same on every machine).
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut s = seed;
        // One step first, so that seeds 0, 1, 2 do not start alike.
        splitmix(&mut s);
        Rng(s)
    }

    /// The generator of one case of the property `name`: the same for the same seed, name and case, whatever else runs.
    pub fn for_case(seed: u64, name: &str, case: u32) -> Self {
        Rng::new(seed ^ fnv(name) ^ (case as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93))
    }

    pub fn next_u64(&mut self) -> u64 {
        splitmix(&mut self.0)
    }

    /// A number below `n` (`n` must not be 0).
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        (self.next_u64() % n as u64) as usize
    }

    /// A number from `lo` to `hi`, both included.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        assert!(lo <= hi, "range({lo}, {hi})");
        lo + self.below(hi - lo + 1)
    }

    /// True `percent` times in a hundred.
    pub fn chance(&mut self, percent: u32) -> bool {
        (self.below(100) as u32) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }

    /// Shuffles `items` in place.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }

    /// A word of lower-case ASCII letters, `lo` to `hi` long.
    pub fn word(&mut self, lo: usize, hi: usize) -> String {
        let n = self.range(lo, hi);
        (0..n).map(|_| (b'a' + self.below(26) as u8) as char).collect()
    }

    /// A character of any plane that is a Unicode scalar value (a lone surrogate is not one: see [`Rng::lossy`]).
    pub fn scalar(&mut self) -> char {
        loop {
            let plane = match self.below(6) {
                0 => self.range(0x20, 0x7e),
                1 => self.range(0xa0, 0x24f),
                2 => self.range(0x900, 0x97f),
                3 => self.range(0x300, 0x36f),
                4 => self.range(0x1f300, 0x1f64f),
                _ => self.range(0, 0x10ffff),
            };
            if let Some(c) = char::from_u32(plane as u32) {
                return c;
            }
        }
    }

    /// Text made of the pieces that break text code: line endings of every kind, NUL, a byte order mark, zero-width and bidirectional marks, combining marks, Devanagari, emoji and
    /// joined emoji, quotes, braces and the other characters a template or a path reads as syntax. `pieces` long at most.
    pub fn nasty(&mut self, pieces: usize) -> String {
        let n = self.range(0, pieces);
        let mut out = String::new();
        for _ in 0..n {
            match self.below(10) {
                0..=4 => out.push_str(self.pick(NASTY)),
                5 | 6 => out.push_str(&self.word(1, 8)),
                7 => out.push(self.scalar()),
                8 => out.push_str(self.pick(&[" ", "  ", "\t", "    ", "\n", "\r\n"])),
                _ => out.push_str(&self.lossy(4)),
            }
        }
        out
    }

    /// `len` random bytes read as UTF-8 with the bad ones turned into U+FFFD, which is how a lone surrogate or a cut character reaches code that takes a `String`.
    pub fn lossy(&mut self, len: usize) -> String {
        let n = self.range(0, len);
        String::from_utf8_lossy(&self.bytes(n)).into_owned()
    }
}

/// The fragments of [`Rng::nasty`].
pub const NASTY: &[&str] = &[
    "", " ", "\t", "\n", "\r", "\r\n", "\n\n", "\0", "\u{feff}", "\u{200b}", "\u{200d}", "\u{202e}", "\u{2028}", "\u{a0}", "\u{3000}", "\u{fffd}", "e\u{301}", "a\u{300}\u{301}\u{302}", "é", "ß", "İ", "ǅ", "नमस्ते", "क्ष", "कि", "ज़", "日本語", "한국어",
    "العربية", "👍", "🎉", "👨\u{200d}👩\u{200d}👧", "🇮🇳", "\"", "'", "`", "\\", "\\\\", "\\n", "\\u00e9", "{", "}", "{{", "}}", "{{ x }}", "${", "$(", "%", "%00", "%2e%2e", "<", ">", "&", "&amp;", "|", ";", "..", "...", "./", "../", "/", "//", "C:", "C:\\", "\\\\?\\", "~", "*", "?", "[", "]",
    "```", "```json", "//", "#", "--", "@@", "@ws/", "http://", "https://example.com/a?b=c#d", "null", "true", "123", "-1", "1e999", "NaN",
];

/// Cuts `data` into pieces of random sizes (1 to `max` bytes, the last one shorter), as a pipe or a socket hands it over.
pub fn chunks<'a>(rng: &mut Rng, data: &'a [u8], max: usize) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let n = rng.range(1, max.max(1)).min(data.len() - at);
        out.push(&data[at..at + n]);
        at += n;
    }
    out
}

/// A mutation of `data` the way a fuzzer makes one: flip a bit, set a byte, insert or delete a run, repeat a piece, cut it off, or put in a word of `dictionary`. One to `steps` of them.
pub fn mutate(rng: &mut Rng, data: &mut Vec<u8>, dictionary: &[&[u8]], steps: usize) {
    for _ in 0..rng.range(1, steps.max(1)) {
        match rng.below(8) {
            0 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
            1 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] = rng.next_u64() as u8;
            }
            2 => {
                let at = rng.below(data.len() + 1);
                let n = rng.range(1, 8);
                let extra = rng.bytes(n);
                data.splice(at..at, extra);
            }
            3 if !data.is_empty() => {
                let from = rng.below(data.len());
                let to = (from + rng.range(1, 16)).min(data.len());
                data.drain(from..to);
            }
            4 if !data.is_empty() => {
                let from = rng.below(data.len());
                let to = (from + rng.range(1, 32)).min(data.len());
                let piece = data[from..to].to_vec();
                let at = rng.below(data.len() + 1);
                data.splice(at..at, piece);
            }
            5 if !data.is_empty() => {
                let keep = rng.below(data.len());
                data.truncate(keep);
            }
            6 if !dictionary.is_empty() => {
                let word = *rng.pick(dictionary);
                let at = rng.below(data.len() + 1);
                data.splice(at..at, word.iter().copied());
            }
            _ => {
                let at = rng.below(data.len() + 1);
                data.insert(at, *rng.pick(&[b'\n', b'\r', 0, b'"', b'{', b'}', b'\\', 0xff, 0xc3]));
            }
        }
        if data.len() > 1 << 16 {
            data.truncate(1 << 16);
        }
    }
}

fn message_of(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(a panic with no text)".to_string()
    }
}

/// Runs `body` for each case of the property `name`: `default_cases` of them, or `AQUA_PROP_CASES`, or only `AQUA_PROP_CASE`. `body` gets the generator of the case and its number,
/// and fails by panicking (`assert!`). A failure is re-raised with the seed and the number of the case in its message, and the line to replay it with.
pub fn check(name: &str, default_cases: u32, mut body: impl FnMut(&mut Rng, u32)) {
    let seed = base_seed();
    let cases = cases_or(default_cases);
    let range = match only_case() {
        Some(i) => i..i.saturating_add(1),
        None => 0..cases,
    };
    let mut ran = 0u32;
    for case in range {
        let mut rng = Rng::for_case(seed, name, case);
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| body(&mut rng, case))) {
            let said = message_of(&*payload);
            eprintln!("PROPERTY FAILED: {name}, case {case} of {cases}, seed {seed:#x}. Replay: AQUA_PROP_SEED={seed:#x} AQUA_PROP_CASE={case}");
            let text = format!("property '{name}' failed at case {case} of {cases} with seed {seed:#x} (replay: AQUA_PROP_SEED={seed:#x} AQUA_PROP_CASE={case}): {said}");
            // Keep the payload's type when it is not a text, so that a `should_panic` or a join on it still sees what it expects.
            if payload.downcast_ref::<&str>().is_some() || payload.downcast_ref::<String>().is_some() {
                resume_unwind(Box::new(text));
            }
            resume_unwind(payload);
        }
        ran += 1;
    }
    assert!(ran > 0, "property '{name}' ran no case (AQUA_PROP_CASES=0?)");
}

/// A seed for a stress test that names its own rounds: the run's seed mixed with the test's name, so that two tests never walk the same dice. A stress test prints it with its
/// own failure.
pub fn seed_for(name: &str) -> u64 {
    base_seed() ^ fnv(name)
}
