//! The full text of a long tool result, kept on disk (card OA5-02; tasks TL-01, CX-04, TM-02 and WF-09 of the OpenCode audit).
//!
//! The host shows the model at most a few thousand characters of a result (`output::head_tail`). Until now the rest was gone for good: the middle of a build log, the failing
//! test of a long run, a page past its first screen. When a result is cut and a [`SpillStore`] is set, the host first writes the **whole redacted text** here and ends the
//! shown text with one line that names the file (`@out/<name>`). The file tools read that folder through a built-in read-only root called `out` (see
//! `PathGuard::set_out_root` in `aqua-native-tools`), so the model can search it with `fs.grep` and page through it with `fs.read`.
//!
//! * **Only redacted text is written.** The host redacts before it shapes (step 10, then 11), and hands this store the redacted text; a secret never reaches the disk through here.
//! * **Nothing the caller chooses is part of the path.** A file is named by the store: a UTC time stamp, a counter, six random hex digits and the tool name with every character
//!   that is not a letter or a digit replaced by `_`. A file is made with `create_new`, so two calls at once never share one.
//! * **The folder is bounded.** A file keeps at most [`SpillConfig::max_file_bytes`] (the start and the end when the text is longer), the folder at most
//!   [`SpillConfig::max_total_bytes`] (the oldest files go first), nothing is written when the drive has less than [`SpillConfig::min_free_bytes`] free (when the caller gave a way to
//!   ask), and [`SpillStore::sweep`] removes what is older than [`SpillConfig::retention`].
//! * **Only files of this shape are ever removed.** A folder the person chose may hold other things; the sweep and the eviction touch a file only when its name has exactly the shape
//!   this store gives (see [`is_spill_name`]), and never a folder or a link.
//! * **A failure is never a failed call.** [`SpillStore::write`] returns an error and the host shows today's head and end with a warning.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The name of the built-in read-only root that shows this folder to the file tools: `@out/<name>`.
pub const ALIAS: &str = "out";

/// How long a file is kept.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The most one file keeps. A longer text is kept as its start and its end.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// The most the folder keeps in all.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;
/// Nothing is written when the drive has less than this free (and the caller gave a way to ask): the disk of this machine is small.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 300 * 1024 * 1024;

/// The limits of a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillConfig {
    pub retention: Duration,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub min_free_bytes: u64,
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self { retention: DEFAULT_RETENTION, max_file_bytes: DEFAULT_MAX_FILE_BYTES, max_total_bytes: DEFAULT_MAX_TOTAL_BYTES, min_free_bytes: DEFAULT_MIN_FREE_BYTES }
    }
}

/// Free bytes on the drive that holds a path, or `None` when that cannot be asked.
pub type FreeSpaceProbe = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// One file that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillRef {
    /// The file name inside the folder.
    pub name: String,
    /// Bytes in the file.
    pub bytes: u64,
    /// Lines in the file.
    pub lines: usize,
    /// SHA-256 of the file, in hex.
    pub sha256: String,
    /// Bytes of the text that was handed over; more than `bytes` when only its start and its end were kept.
    pub original_bytes: u64,
}

impl SpillRef {
    /// The path the model uses: `@out/<name>`.
    pub fn alias_path(&self) -> String {
        format!("@{ALIAS}/{}", self.name)
    }

    /// Was the whole text kept?
    pub fn whole(&self) -> bool {
        self.bytes >= self.original_bytes
    }

    /// The last line of a cut result: where the whole text is and how to use it. The tools it names are the ones the model has for a file; their arguments are the real ones.
    pub fn pointer_line(&self) -> String {
        let path = self.alias_path();
        let kept = if self.whole() { String::new() } else { format!(" (the start and the end of {} bytes)", self.original_bytes) };
        format!(
            "[Aqua: output cut. Full text kept: {} lines, {} bytes{kept}, as {path}. Search it: fs.grep pattern=\"...\" path={path}. Read a part: fs.read path={path} offset_line=N max_lines=60]",
            self.lines, self.bytes
        )
    }
}

/// What a sweep removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: usize,
    pub bytes: u64,
}

struct Stored {
    path: PathBuf,
    name: String,
    bytes: u64,
    modified: SystemTime,
}

pub struct SpillStore {
    dir: PathBuf,
    cfg: SpillConfig,
    seq: AtomicU64,
    free_space: RwLock<Option<FreeSpaceProbe>>,
    /// One write at a time: making room and making the file are one step.
    writing: Mutex<()>,
}

impl SpillStore {
    /// A store in `dir` (made when it is not there) with the default limits.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::with_config(dir, SpillConfig::default())
    }

    pub fn with_config(dir: impl Into<PathBuf>, cfg: SpillConfig) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        if !std::fs::metadata(&dir)?.is_dir() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} is not a folder", dir.display())));
        }
        Ok(Self { dir, cfg, seq: AtomicU64::new(0), free_space: RwLock::new(None), writing: Mutex::new(()) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config(&self) -> SpillConfig {
        self.cfg
    }

    /// How the store asks how much room the drive has (the core gives one; without it the check is skipped).
    pub fn set_free_space_probe(&self, probe: FreeSpaceProbe) {
        *self.free_space.write().unwrap_or_else(PoisonError::into_inner) = Some(probe);
    }

    /// Keep `text` (which the caller has already redacted) as a new file. `tool` becomes part of the file name, in letters and digits only.
    pub fn write(&self, tool: &str, text: &str) -> std::io::Result<SpillRef> {
        let original_bytes = text.len() as u64;
        // A file is never bigger than the folder may be: a limit set low for a test or a small disk still keeps the start and the end of a long text.
        let (kept, whole) = fit(text, self.cfg.max_file_bytes.min(self.cfg.max_total_bytes));
        let data = kept.as_bytes();
        let _one_at_a_time = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(probe) = self.free_space.read().unwrap_or_else(PoisonError::into_inner).clone() {
            if let Some(free) = probe(&self.dir) {
                if free < self.cfg.min_free_bytes.saturating_add(data.len() as u64) {
                    return Err(std::io::Error::other(format!("the drive has only {} MB free, so the full output was not kept", free / (1024 * 1024))));
                }
            }
        }
        self.make_room(data.len() as u64)?;
        let stamp = utc_stamp(SystemTime::now());
        let tool_part = tool_part(tool);
        let mut last_error = None;
        for _ in 0..8 {
            let name = format!("{stamp}_{}_{}_{tool_part}.txt", self.seq.fetch_add(1, Ordering::SeqCst), random_hex());
            let path = self.dir.join(&name);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let written = file.write_all(data).and_then(|_| file.sync_data());
                    if let Err(e) = written {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        return Err(e);
                    }
                    return Ok(SpillRef {
                        name,
                        bytes: data.len() as u64,
                        lines: count_lines(&kept),
                        sha256: Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect(),
                        original_bytes: if whole { data.len() as u64 } else { original_bytes },
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_error.unwrap_or_else(|| std::io::Error::other("no free file name")))
    }

    /// Remove what is older than the retention. Only files of this store's own shape; never a folder or a link.
    pub fn sweep(&self) -> SweepReport {
        self.sweep_at(SystemTime::now())
    }

    /// [`sweep`](Self::sweep) as of `now` (a test gives a later time instead of waiting a week).
    pub fn sweep_at(&self, now: SystemTime) -> SweepReport {
        let _one_at_a_time = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut report = SweepReport::default();
        for f in self.files() {
            let old = now.duration_since(f.modified).is_ok_and(|age| age >= self.cfg.retention);
            if old && std::fs::remove_file(&f.path).is_ok() {
                report.removed += 1;
                report.bytes += f.bytes;
            }
        }
        report
    }

    /// The bytes the folder holds now (only files of this store's shape).
    pub fn total_bytes(&self) -> u64 {
        self.files().iter().map(|f| f.bytes).sum()
    }

    /// The names of the files in the folder, oldest first.
    pub fn names(&self) -> Vec<String> {
        let mut files = self.files();
        files.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.name.cmp(&b.name)));
        files.into_iter().map(|f| f.name).collect()
    }

    /// Does this call read a file of this folder (a path in its arguments that begins `@out/` or is inside the folder)? The result of such a call is not kept again: the
    /// file is already on disk and the model pages it with `offset_line`.
    pub fn is_read_of(&self, args: &Value) -> bool {
        let dir = normal(&self.dir.to_string_lossy());
        let mine = |s: &str| {
            let n = normal(s);
            let alias = format!("@{ALIAS}");
            n == alias || n.starts_with(&format!("{alias}/")) || (!dir.is_empty() && (n == dir || n.starts_with(&format!("{dir}/"))))
        };
        match args {
            Value::String(s) => mine(s),
            Value::Array(a) => a.iter().any(|v| self.is_read_of(v)),
            Value::Object(o) => o.iter().any(|(k, v)| matches!(k.as_str(), "path" | "paths" | "file" | "files" | "src" | "dst" | "dir" | "cwd") && self.is_read_of(v)),
            _ => false,
        }
    }

    fn files(&self) -> Vec<Stored> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
            if !is_spill_name(&name) {
                continue;
            }
            // `metadata` of a directory entry does not follow a link: a link is never counted as a file of this store, and so never removed.
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            out.push(Stored { path: entry.path(), name, bytes: meta.len(), modified: meta.modified().unwrap_or(UNIX_EPOCH) });
        }
        out
    }

    /// Make room for `incoming` bytes: the oldest files go until the folder, with the new file, is within its limit.
    fn make_room(&self, incoming: u64) -> std::io::Result<()> {
        let mut files = self.files();
        let mut total: u64 = files.iter().map(|f| f.bytes).sum();
        if total.saturating_add(incoming) <= self.cfg.max_total_bytes {
            return Ok(());
        }
        files.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.name.cmp(&b.name)));
        for f in files {
            if total.saturating_add(incoming) <= self.cfg.max_total_bytes {
                break;
            }
            // A file that cannot be removed now (a program holds it open) is skipped; the next one is tried.
            if std::fs::remove_file(&f.path).is_ok() {
                total -= f.bytes;
            }
        }
        if total.saturating_add(incoming) > self.cfg.max_total_bytes {
            return Err(std::io::Error::other("the folder of kept output is full and nothing in it could be removed"));
        }
        Ok(())
    }
}

/// `text` as one file keeps it: the whole text when it fits, else its start (a quarter) and its end (three quarters) with a line between that says how much is missing. The
/// cuts fall on character boundaries. The flag says whether the whole text was kept.
fn fit(text: &str, max_bytes: u64) -> (std::borrow::Cow<'_, str>, bool) {
    let max = max_bytes as usize;
    if text.len() <= max {
        return (std::borrow::Cow::Borrowed(text), true);
    }
    let note_room = 160;
    let room = max.saturating_sub(note_room).max(1);
    let (mut head_end, mut tail_start) = (room / 4, text.len() - (room - room / 4));
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let missing = tail_start.saturating_sub(head_end);
    let kept = format!("{}\n[... {missing} bytes of the output are not kept here: this file holds the first {head_end} and the last {} bytes ...]\n{}", &text[..head_end], text.len() - tail_start, &text[tail_start..]);
    (std::borrow::Cow::Owned(kept), false)
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.matches('\n').count() + usize::from(!text.ends_with('\n'))
}

/// The part of a file name that comes from the tool: lower-case letters and digits, every other character `_`, at most 32.
fn tool_part(tool: &str) -> String {
    let s: String = tool.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).take(32).collect();
    if s.is_empty() {
        "tool".into()
    } else {
        s
    }
}

fn random_hex() -> String {
    let mut b = [0u8; 3];
    if getrandom::getrandom(&mut b).is_err() {
        static COUNTER: AtomicU64 = AtomicU64::new(0x9e37);
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let mixed = t ^ COUNTER.fetch_add(0x9e37_79b9, Ordering::SeqCst) ^ u64::from(std::process::id()).rotate_left(17);
        b.copy_from_slice(&mixed.to_le_bytes()[..3]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `YYYYMMDD_HHMMSS` in UTC.
fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The calendar date of a day number counted from 1970-01-01 (the proleptic Gregorian calendar).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Is this exactly a name that [`SpillStore::write`] gives: `YYYYMMDD_HHMMSS_<counter>_<six hex digits>_<tool>.txt`? Nothing else in the folder is ever removed.
pub fn is_spill_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".txt") else { return false };
    let mut parts = stem.splitn(5, '_');
    let (Some(date), Some(time), Some(seq), Some(rand), Some(tool)) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else { return false };
    date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
        && time.len() == 6
        && time.bytes().all(|b| b.is_ascii_digit())
        && !seq.is_empty()
        && seq.bytes().all(|b| b.is_ascii_digit())
        && rand.len() == 6
        && rand.bytes().all(|b| b.is_ascii_hexdigit())
        && !tool.is_empty()
        && tool.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A path as it is compared: forward slashes, lower case (Windows ignores case), no closing slash.
fn normal(s: &str) -> String {
    s.trim().replace('\\', "/").trim_end_matches('/').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_come_out_right_across_leap_days_and_the_epoch() {
        for (secs, expect) in [(0u64, "19700101_000000"), (951_782_400, "20000229_000000"), (1_781_007_045, "20260609_121045"), (1_709_164_799, "20240228_235959"), (1_709_164_800, "20240229_000000")] {
            assert_eq!(utc_stamp(UNIX_EPOCH + Duration::from_secs(secs)), expect, "{secs}");
        }
    }

    #[test]
    fn only_names_of_the_shape_the_store_gives_count_as_its_files() {
        for good in ["20261009_153012_3_a1b2c3_term_run.txt", "20261009_153012_0_00ff00_tool.txt", "19700101_000000_12345_abcdef_x9.txt"] {
            assert!(is_spill_name(good), "{good}");
        }
        for bad in [
            "notes.txt",
            "20261009_153012_3_a1b2c3_term_run.log",
            "20261009_153012_3_a1b2c_term_run.txt",
            "2026100_153012_3_a1b2c3_term_run.txt",
            "20261009_153012__a1b2c3_term_run.txt",
            "20261009_153012_3_a1b2c3_.txt",
            "20261009_153012_3_a1b2c3_Term.Run.txt",
            "20261009_153012_3_g1b2c3_term.txt",
            "readme.md",
            "",
        ] {
            assert!(!is_spill_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_tool_name_becomes_letters_and_digits_and_nothing_else() {
        assert_eq!(tool_part("term.run"), "term_run");
        assert_eq!(tool_part("../..\\evil:name"), "______evil_name");
        assert_eq!(tool_part(""), "tool");
        assert_eq!(tool_part(&"a".repeat(100)).len(), 32);
        assert!(is_spill_name(&format!("20261009_153012_3_a1b2c3_{}.txt", tool_part("MCP.Server/Tool Name"))));
    }

    #[test]
    fn a_text_over_the_limit_keeps_its_start_and_its_end_on_character_boundaries() {
        let text: String = (0..5000).map(|i| ['e', '\u{e9}', '\u{65e5}', '\u{1f642}'][i % 4]).collect();
        let (kept, whole) = fit(&text, 1000);
        assert!(!whole && kept.len() <= 1000, "{}", kept.len());
        assert!(kept.contains("bytes of the output are not kept here"));
        let (first, last): (String, String) = (text.chars().take(2).collect(), text.chars().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect());
        assert!(kept.starts_with(&first) && kept.ends_with(&last));
        let (same, whole) = fit("short", 1000);
        assert!(whole && same == "short");
    }

    #[test]
    fn lines_are_counted_the_way_an_editor_counts_them() {
        assert_eq!((count_lines(""), count_lines("a"), count_lines("a\n"), count_lines("a\nb"), count_lines("a\n\nb\n")), (0, 1, 1, 2, 3));
    }

    #[test]
    fn a_call_that_reads_the_folder_is_recognised_by_its_arguments() {
        let dir = std::env::temp_dir().join(format!("aqua_spill_args_{}", std::process::id()));
        let store = SpillStore::open(&dir).unwrap();
        let reads = |v: serde_json::Value| store.is_read_of(&v);
        assert!(reads(serde_json::json!({"path": "@out/20261009_153012_3_a1b2c3_term_run.txt", "pattern": "x"})));
        assert!(reads(serde_json::json!({"path": "@OUT\\x.txt"})));
        assert!(reads(serde_json::json!({"path": "@out"})));
        assert!(reads(serde_json::json!({"path": dir.join("x.txt").to_string_lossy()})));
        assert!(!reads(serde_json::json!({"path": "@outside/x.txt"})));
        assert!(!reads(serde_json::json!({"path": "@ws/out/x.txt", "pattern": "@out/"})));
        assert!(!reads(serde_json::json!({"command": "echo @out/x"})));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
