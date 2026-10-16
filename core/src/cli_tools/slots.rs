//! `aqua-cli slots list | forget`: see and delete the saved model slots (card OA9-06, the "forget" command its page said was not built).
//!
//! With `AQUA_LLAMA_SLOT_SAVE=on` the model server's slot, the state it holds for the start of the agent's prompt, is saved as a file `kv_<16 hex>_<16 hex>.bin` in `data/cache/kv`
//! (`modules::llm_router::kv_slots`). **Such a file holds the tokens of a conversation and what the model made of them, so it is as private as the conversation.** These two commands let a
//! person see what is saved and delete all of it without a running core and without the switch being on (it may have been turned off after the files were made).
//!
//! * `list` prints how many files there are, their size in all, and for each file its name, size and age. Never a content and never a path.
//! * `forget` deletes every saved slot file and says how many files and bytes went. The folder itself stays.
//!
//! **What they touch.** Only the folder `cache/kv` under the data folder, and in it only files whose name is exactly the form the saver makes ([`is_slot_file_name`]) and that are plain files
//! (not links, not folders). A link or junction at `cache` or `kv` that leaves the data folder is not followed: the command says so and does nothing. A folder, a link, or a file with any other
//! name inside the folder is left alone and only counted. A file that cannot be deleted (in use, read-only) is named, the others are still tried, and the command ends non-zero. A file is
//! never opened: the commands read names, sizes and times from the directory and nothing else.
//!
//! **No module edge.** The rules of the saver (the folder and the name) are private copies here, so that `cli_tools` does not use `modules`; the tests hold the two to the same answers, the way
//! `door.rs` keeps its own reader of the comparison file.

use crate::headless::render::plain;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The two folders under the data folder that hold the saved slots, the way the saver names them (`kv_slots::slot_dir`).
const FOLDER_PARTS: [&str; 2] = ["cache", "kv"];
/// How the folder is shown: relative to the data folder, so that no path of this computer is printed.
const FOLDER_SHOWN: &str = "cache/kv";

/// Is this a name the saver makes (`kv_` and 16 hex digits, `_` and 16 hex digits, `.bin`)? The same rule as `kv_slots::is_slot_file_name`; a test holds the two together.
pub fn is_slot_file_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("kv_").and_then(|r| r.strip_suffix(".bin")) else { return false };
    let Some((model, prefix)) = rest.split_once('_') else { return false };
    let hex16 = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    hex16(model) && hex16(prefix)
}

/// What is at the place of the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Folder {
    /// Nothing is there (no data folder, no `cache` or no `kv` in it): there are no saved slots, and nothing is made.
    Missing,
    /// There is something that must not be used (a link that leaves the data folder, a file where a folder should be, a folder that cannot be read). The words say what; no path.
    Refused(String),
    /// The folder with links followed, inside the data folder. For reading and deleting; never printed.
    Found(PathBuf),
}

fn nothing_done() -> &'static str {
    "so nothing was read or deleted"
}

/// Finds the folder of saved slots under `data` the way `kv_slots::ensure_slot_dir` decides it, but makes nothing: one level at a time, links followed, and a level that leaves the data
/// folder (or cannot be followed) refuses the whole.
pub fn find_folder(data: &Path) -> Folder {
    let root = match std::fs::canonicalize(data) {
        Ok(root) => root,
        Err(e) if e.kind() == ErrorKind::NotFound => return Folder::Missing,
        Err(e) => return Folder::Refused(format!("the data folder cannot be read ({:?}), {}", e.kind(), nothing_done())),
    };
    if !root.is_dir() {
        return Folder::Refused(format!("the data folder is not a folder, {}", nothing_done()));
    }
    let mut at = root.clone();
    for part in FOLDER_PARTS {
        let next = at.join(part);
        match std::fs::symlink_metadata(&next) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Folder::Missing,
            Err(e) => return Folder::Refused(format!("{FOLDER_SHOWN} cannot be read ({:?}), {}", e.kind(), nothing_done())),
        }
        match std::fs::canonicalize(&next) {
            Ok(real) if real.starts_with(&root) => {
                if !real.is_dir() {
                    return Folder::Refused(format!("{FOLDER_SHOWN} is not a folder, {}", nothing_done()));
                }
                at = real;
            }
            _ => return Folder::Refused(format!("{FOLDER_SHOWN} is, or is behind, a link that leaves the data folder or cannot be followed: it was not followed, {}", nothing_done())),
        }
    }
    Folder::Found(at)
}

/// A plain file: not a link, not a folder, and on Windows not a reparse point of any kind (a junction, a symbolic link or a placeholder). `None` for everything else.
fn plain_file(path: &Path) -> Option<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    (meta.file_type().is_file() && !is_reparse_point(&meta)).then_some(meta)
}

#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// One saved slot file: its name, its size and the time it was written (from the directory; the content is never read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotFile {
    pub name: String,
    pub bytes: u64,
    pub modified: Option<SystemTime>,
}

/// What is in the folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// The saved slot files, by name.
    pub files: Vec<SlotFile>,
    /// Entries that are not saved slot files (a folder, a link, a name the saver did not make, an entry that cannot be looked at): counted, never named, read or followed.
    pub left_alone: usize,
}

impl Scan {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().fold(0u64, |sum, f| sum.saturating_add(f.bytes))
    }
}

/// Looks at the entries of the folder `dir` (as `find_folder` gave it). Reads names, kinds, sizes and times only.
pub fn scan(dir: &Path) -> Result<Scan, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{FOLDER_SHOWN} cannot be read ({:?}), {}", e.kind(), nothing_done()))?;
    let mut out = Scan::default();
    for entry in read {
        let Ok(entry) = entry else {
            out.left_alone += 1;
            continue;
        };
        let name = match entry.file_name().to_str() {
            Some(name) if is_slot_file_name(name) => name.to_string(),
            _ => {
                out.left_alone += 1;
                continue;
            }
        };
        match plain_file(&dir.join(&name)) {
            Some(meta) => out.files.push(SlotFile { name, bytes: meta.len(), modified: meta.modified().ok() }),
            None => out.left_alone += 1,
        }
    }
    out.files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// What a command prints and how it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub out: String,
    pub err: String,
    pub code: i32,
}

impl Report {
    fn ok(out: String) -> Report {
        Report { out, err: String::new(), code: 0 }
    }
    fn refused(command: &str, why: &str) -> Report {
        Report { out: String::new(), err: format!("slots {command}: {}\n", plain(why)), code: 1 }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A size in the unit a person reads (binary units, one decimal).
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("TB"))
}

/// A count with thousands separators, so that the exact bytes can be read.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn age_text(modified: Option<SystemTime>, now_secs: u64) -> String {
    let Some(written) = modified.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()) else { return "age not known".into() };
    let s = now_secs.saturating_sub(written);
    match s {
        0..=89 => format!("{s} seconds ago"),
        90..=5_399 => format!("{} minutes ago", s / 60),
        5_400..=129_599 => format!("{} hours ago", s / 3_600),
        _ => format!("{} days ago", s / 86_400),
    }
}

fn left_alone_line(n: usize) -> String {
    format!("left alone: {} (a folder, a link, or a name the saver did not make; they are never read, followed or deleted)\n", plural(n, "entry", "entries"))
}

/// `aqua-cli slots list`: how many saved slot files there are, their size, and each one's name, size and age. `now_secs` is the clock of the program.
pub fn list(data: &Path, now_secs: u64) -> Report {
    let dir = match find_folder(data) {
        Folder::Missing => return Report::ok("no saved slots\n".into()),
        Folder::Refused(why) => return Report::refused("list", &why),
        Folder::Found(dir) => dir,
    };
    let scan = match scan(&dir) {
        Ok(scan) => scan,
        Err(why) => return Report::refused("list", &why),
    };
    list_text(&scan, now_secs)
}

/// The words of `list` for what was found.
pub fn list_text(scan: &Scan, now_secs: u64) -> Report {
    let mut out = String::new();
    if scan.files.is_empty() {
        out.push_str("no saved slots\n");
    } else {
        let total = scan.total_bytes();
        out.push_str(&format!("{} in {FOLDER_SHOWN}: {} bytes ({}) in all\n", plural(scan.files.len(), "saved slot file", "saved slot files"), grouped(total), human(total)));
        for f in &scan.files {
            out.push_str(&format!("  {}  {:>9}  {}\n", f.name, human(f.bytes), age_text(f.modified, now_secs)));
        }
    }
    if scan.left_alone > 0 {
        out.push_str(&left_alone_line(scan.left_alone));
    }
    Report::ok(out)
}

/// What `forget` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forgotten {
    pub files: usize,
    pub bytes: u64,
    /// Files that could not be deleted: the name and why.
    pub failed: Vec<(String, String)>,
    pub left_alone: usize,
}

/// `aqua-cli slots forget`: deletes every saved slot file in the folder, and nothing else.
pub fn forget(data: &Path) -> Report {
    forget_with(data, &|path: &Path| std::fs::remove_file(path))
}

/// `forget` with the deletion given (a test makes some files refuse; the command gives `std::fs::remove_file`, which never follows a link at the last name).
pub fn forget_with(data: &Path, remove: &dyn Fn(&Path) -> std::io::Result<()>) -> Report {
    let dir = match find_folder(data) {
        Folder::Missing => return Report::ok("no saved slots\n".into()),
        Folder::Refused(why) => return Report::refused("forget", &why),
        Folder::Found(dir) => dir,
    };
    let scan = match scan(&dir) {
        Ok(scan) => scan,
        Err(why) => return Report::refused("forget", &why),
    };
    let mut result = Forgotten { left_alone: scan.left_alone, ..Forgotten::default() };
    for f in &scan.files {
        let path = dir.join(&f.name);
        // Looked at again just before: a name that has become a link or a folder since the scan is left alone.
        if plain_file(&path).is_none() {
            result.left_alone += 1;
            continue;
        }
        match remove(&path) {
            Ok(()) => {
                result.files += 1;
                result.bytes = result.bytes.saturating_add(f.bytes);
            }
            // Gone already (the model server's own pruning, or the person): nothing for this command to report.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => result.failed.push((f.name.clone(), plain(&e.to_string().chars().take(160).collect::<String>()))),
        }
    }
    forget_text(&result)
}

/// The words of `forget` for what was done: what went on standard output, the files that stayed on standard error, and the code (1 when a file could not be deleted).
pub fn forget_text(done: &Forgotten) -> Report {
    let mut out = String::new();
    if done.files == 0 && done.failed.is_empty() {
        out.push_str("no saved slots\n");
    } else if done.files > 0 {
        out.push_str(&format!("deleted {}, {} bytes ({}) from {FOLDER_SHOWN}\n", plural(done.files, "saved slot file", "saved slot files"), grouped(done.bytes), human(done.bytes)));
    } else {
        out.push_str("deleted 0 saved slot files\n");
    }
    if done.left_alone > 0 {
        out.push_str(&left_alone_line(done.left_alone));
    }
    let mut err = String::new();
    for (name, why) in &done.failed {
        err.push_str(&format!("slots forget: {name} could not be deleted: {why}\n"));
    }
    if !done.failed.is_empty() {
        err.push_str(&format!("slots forget: {} stayed (the others were deleted); close what uses them or clear their read-only mark, then run it again\n", plural(done.failed.len(), "saved slot file", "saved slot files")));
    }
    Report { out, err, code: i32::from(!done.failed.is_empty()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::llm_router::kv_slots;
    use std::cell::RefCell;
    use std::fs;

    /// A folder of its own under the worktree's `target`, with `data` and `outside` in it; the links a test made are taken away first, so that nothing is ever deleted through one.
    struct World {
        root: PathBuf,
        data: PathBuf,
        outside: PathBuf,
        links: RefCell<Vec<PathBuf>>,
    }

    impl World {
        fn new(name: &str) -> World {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_cli_tools").join(format!("slots_{name}_{}", std::process::id()));
            let world = World { data: root.join("data"), outside: root.join("outside"), root, links: RefCell::new(Vec::new()) };
            world.clean();
            fs::create_dir_all(&world.data).unwrap();
            fs::create_dir_all(&world.outside).unwrap();
            world
        }
        fn kv(&self) -> PathBuf {
            self.data.join("cache").join("kv")
        }
        fn make_kv(&self) -> PathBuf {
            fs::create_dir_all(self.kv()).unwrap();
            self.kv()
        }
        fn clean(&self) {
            for link in self.links.borrow().iter() {
                if fs::remove_dir(link).is_err() {
                    let _ = fs::remove_file(link);
                }
            }
            self.links.borrow_mut().clear();
            if self.root.exists() {
                // Files a test made read-only are made writable again first.
                fn writable(dir: &Path) {
                    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                        let path = entry.path();
                        if let Ok(meta) = fs::symlink_metadata(&path) {
                            if meta.file_type().is_dir() {
                                writable(&path);
                            } else if meta.file_type().is_file() {
                                let mut p = meta.permissions();
                                #[allow(clippy::permissions_set_readonly_false)]
                                p.set_readonly(false);
                                let _ = fs::set_permissions(&path, p);
                            }
                        }
                    }
                }
                writable(&self.root);
                let _ = fs::remove_dir_all(&self.root);
            }
        }
        /// A directory link (a junction on Windows, which needs no privilege; a symbolic link elsewhere). `false` when this computer will not make one.
        fn dir_link(&self, link: &Path, target: &Path) -> bool {
            let made = if cfg!(windows) {
                std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().map(|o| o.status.success()).unwrap_or(false)
            } else {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(target, link).is_ok()
                }
                #[cfg(not(unix))]
                {
                    false
                }
            };
            if made {
                self.links.borrow_mut().push(link.to_path_buf());
            } else {
                eprintln!("SKIPPED part of the test: this computer would not make a directory link at {}", link.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
            }
            made
        }
        /// A symbolic link to a file. `false` when this computer will not make one (Windows needs a privilege for it).
        fn file_link(&self, link: &Path, target: &Path) -> bool {
            #[cfg(windows)]
            let made = std::os::windows::fs::symlink_file(target, link).is_ok();
            #[cfg(unix)]
            let made = std::os::unix::fs::symlink(target, link).is_ok();
            #[cfg(not(any(windows, unix)))]
            let made = false;
            if made {
                self.links.borrow_mut().push(link.to_path_buf());
            } else {
                eprintln!("SKIPPED part of the test: this computer would not make a file link at {}", link.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
            }
            made
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            self.clean();
        }
    }

    fn slot_name(model: u64, prefix: u64) -> String {
        format!("kv_{model:016x}_{prefix:016x}.bin")
    }

    const MARKER: &str = "WHAT-THE-PERSON-SAID-IN-THE-CONVERSATION";

    /// A saved slot file of exactly `bytes` bytes made of the marker (the commands must never print it), written at `written_secs` since 1970.
    fn put_slot(dir: &Path, name: &str, bytes: usize, written_secs: u64) {
        let content: Vec<u8> = MARKER.bytes().cycle().take(bytes).collect();
        fs::write(dir.join(name), &content).unwrap();
        let file = fs::OpenOptions::new().write(true).open(dir.join(name)).unwrap();
        file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(written_secs)).unwrap();
    }

    /// Every entry under `dir` with its size, recursively and without following a link: what a test compares to say that a command touched nothing there.
    fn listing(dir: &Path) -> Vec<(String, u64, bool)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64, bool)>) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&path) else { continue };
                let shown = path.strip_prefix(base).unwrap_or(&path).display().to_string();
                out.push((shown, if meta.file_type().is_file() { meta.len() } else { 0 }, meta.file_type().is_symlink()));
                if meta.file_type().is_dir() {
                    walk(base, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    const NOW: u64 = 1_791_000_000;

    #[test]
    fn the_rules_for_the_name_and_the_folder_are_the_savers_own() {
        // The names the saver makes are names here.
        for (file, size, time, prefix) in [("model.gguf", 5_000_000_000u64, 1_780_000_000u64, "the start of a prompt"), ("Qwen3-8B-Q4_K_M.gguf", 1, 0, ""), ("x", u64::MAX, u64::MAX, "\u{e9}")] {
            let made = kv_slots::file_name_for(file, size, time, prefix);
            assert!(is_slot_file_name(&made) && kv_slots::is_slot_file_name(&made), "{made}");
        }
        // And for every name near a good one, the two rules say the same.
        let good = slot_name(0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);
        assert!(is_slot_file_name(&good));
        let mut names: Vec<String> = vec![String::new(), "kv_.bin".into(), "kv__.bin".into(), "notes.txt".into(), "kv_short_short.bin".into(), "../kv_0123456789abcdef_fedcba9876543210.bin".into(), format!("{good} "), format!(" {good}"), format!("{good}.bak"), good.to_uppercase(), good.replace(".bin", ".BIN"), good.replace('_', "-"), format!("{}\\x", good), format!("sub/{good}")];
        for i in 0..good.chars().count() {
            for replacement in ['g', 'G', '_', '.', '/', '\\', '0', 'F', 'a', ' ', '\u{e9}', '\u{ff10}', '\0'] {
                names.push(good.chars().enumerate().map(|(j, c)| if i == j { replacement } else { c }).collect());
            }
            names.push(good.chars().enumerate().filter(|(j, _)| *j != i).map(|(_, c)| c).collect());
            names.push(good.chars().enumerate().flat_map(|(j, c)| if i == j { vec!['0', c] } else { vec![c] }).collect());
        }
        let mut accepted = 0;
        for name in &names {
            assert_eq!(is_slot_file_name(name), kv_slots::is_slot_file_name(name), "{name:?}");
            accepted += usize::from(is_slot_file_name(name));
        }
        assert!(accepted > 0 && accepted < names.len(), "the sweep holds names of both kinds ({accepted} of {})", names.len());
        // The folder is the saver's folder.
        let data = Path::new("some").join("data");
        assert_eq!(kv_slots::slot_dir(&data), FOLDER_PARTS.iter().fold(data.clone(), |p, part| p.join(part)));
        assert_eq!(FOLDER_PARTS.join("/"), FOLDER_SHOWN);
    }

    #[test]
    fn list_counts_only_saved_slot_files_and_prints_a_name_a_size_and_an_age_and_never_a_content_or_a_path() {
        let w = World::new("list");
        let kv = w.make_kv();
        let (a, b) = (slot_name(1, 2), slot_name(3, 4));
        put_slot(&kv, &a, 100, NOW - 7_200);
        put_slot(&kv, &b, 3_000_000, NOW - 30);
        // What is not a saved slot file: a wrong name, a folder, a folder with a slot name that holds a slot-named file, and a name with a capital.
        put_slot(&kv, "notes.txt", 10, NOW);
        fs::create_dir(kv.join("sub")).unwrap();
        fs::create_dir(kv.join(slot_name(5, 6))).unwrap();
        put_slot(&kv.join(slot_name(5, 6)), &slot_name(7, 8), 50, NOW);
        put_slot(&kv.join("sub"), &slot_name(9, 10), 50, NOW);
        put_slot(&kv, "kv_0000000000000001_000000000000000A.bin", 10, NOW);
        let before = listing(&w.root);

        let report = list(&w.data, NOW);
        assert_eq!((report.code, report.err.as_str()), (0, ""), "{report:?}");
        let out = &report.out;
        assert!(out.starts_with("2 saved slot files in cache/kv: 3,000,100 bytes (2.9 MB) in all\n"), "{out}");
        assert!(out.lines().any(|l| l.contains(&a) && l.contains("100 B") && l.ends_with("2 hours ago")), "{out}");
        assert!(out.lines().any(|l| l.contains(&b) && l.contains("2.9 MB") && l.ends_with("30 seconds ago")), "{out}");
        assert!(out.contains("left alone: 4 entries"), "{out}");
        assert_eq!(out.lines().count(), 4, "one line for the total, one for each file, one for the rest: {out}");
        // No content, no name of what was left alone, no path of this computer.
        for forbidden in [MARKER, "notes.txt", "sub", &slot_name(7, 8), &slot_name(9, 10), "target", "scratch", "\\", ":/"] {
            assert!(!out.contains(forbidden), "{forbidden}: {out}");
        }
        assert_eq!(listing(&w.root), before, "list changed nothing");
        // One file says "1 saved slot file".
        fs::remove_file(kv.join(&b)).unwrap();
        assert!(list(&w.data, NOW).out.starts_with("1 saved slot file in cache/kv: 100 bytes (100 B) in all\n"));
    }

    #[test]
    fn a_folder_that_is_not_there_is_no_saved_slots_and_nothing_is_made() {
        let w = World::new("missing");
        let nowhere = w.root.join("not_a_data_folder");
        let check = |data: &Path| {
            let before = listing(&w.root);
            for report in [list(data, NOW), forget(data)] {
                assert_eq!((report.out.as_str(), report.err.as_str(), report.code), ("no saved slots
", "", 0), "{report:?}");
            }
            assert_eq!(listing(&w.root), before, "nothing was made");
        };
        check(&nowhere);
        check(&w.data);
        fs::create_dir_all(w.data.join("cache")).unwrap();
        check(&w.data);
        assert!(!nowhere.exists() && !w.kv().exists());
        // An empty folder is the same, and it stays.
        w.make_kv();
        check(&w.data);
        assert!(w.kv().is_dir());
    }

    #[test]
    fn forget_deletes_exactly_the_saved_slot_files_reports_the_rest_and_keeps_the_folder() {
        let w = World::new("forget");
        let kv = w.make_kv();
        let (a, b) = (slot_name(1, 2), slot_name(3, 4));
        put_slot(&kv, &a, 100, NOW - 7_200);
        put_slot(&kv, &b, 1_500, NOW - 30);
        put_slot(&kv, "notes.txt", 10, NOW);
        put_slot(&kv, "kv_0123456789abcdef_0123456789abcdef.bin.old", 10, NOW);
        fs::create_dir(kv.join(slot_name(5, 6))).unwrap();
        put_slot(&kv.join(slot_name(5, 6)), &slot_name(7, 8), 50, NOW);
        // Something beside the folder is not looked at.
        put_slot(&w.data.join("cache"), &slot_name(11, 12), 20, NOW);
        put_slot(&w.data, &slot_name(13, 14), 20, NOW);
        let other_before: Vec<_> = listing(&w.root).into_iter().filter(|(p, _, _)| !p.ends_with(&a) && !p.ends_with(&b)).collect();

        let report = forget(&w.data);
        assert_eq!((report.code, report.err.as_str()), (0, ""), "{report:?}");
        assert!(report.out.starts_with("deleted 2 saved slot files, 1,600 bytes (1.6 KB) from cache/kv\n"), "{}", report.out);
        assert!(report.out.contains("left alone: 3 entries"), "{}", report.out);
        assert!(!report.out.contains(MARKER) && !report.out.contains("notes.txt"), "{}", report.out);
        assert!(!kv.join(&a).exists() && !kv.join(&b).exists());
        assert_eq!(listing(&w.root), other_before, "nothing but the two files went");
        assert!(kv.is_dir(), "the folder stays");
        // A second run finds nothing to delete, still leaves the rest alone, and still succeeds.
        let again = forget(&w.data);
        assert_eq!((again.code, again.err.as_str()), (0, ""));
        assert!(again.out.starts_with("no saved slots\n") && again.out.contains("left alone: 3 entries"), "{}", again.out);
        assert_eq!(listing(&w.root), other_before);
        let after_list = list(&w.data, NOW);
        assert!(after_list.out.starts_with("no saved slots\n"), "{}", after_list.out);
        // One file is "1 saved slot file".
        put_slot(&kv, &a, 5, NOW);
        assert!(forget(&w.data).out.starts_with("deleted 1 saved slot file, 5 bytes (5 B) from cache/kv\n"));
    }

    #[test]
    fn a_link_at_cache_or_kv_that_leaves_the_data_folder_is_refused_and_nothing_outside_is_touched() {
        let w = World::new("outside_link");
        // What lies outside, and must come out of every run as it went in: slot-named files, in the place a followed link would lead to.
        put_slot(&w.outside, &slot_name(1, 1), 40, NOW);
        fs::create_dir_all(w.outside.join("kv")).unwrap();
        put_slot(&w.outside.join("kv"), &slot_name(2, 2), 40, NOW);
        let outside_before = listing(&w.outside);
        fs::create_dir_all(w.data.join("cache")).unwrap();

        // A link at `kv`.
        if w.dir_link(&w.kv(), &w.outside) {
            for report in [list(&w.data, NOW), forget(&w.data)] {
                assert_eq!((report.code, report.out.as_str()), (1, ""), "{report:?}");
                assert!(report.err.contains("link that leaves the data folder") && report.err.contains("nothing was read or deleted"), "{}", report.err);
                for forbidden in ["outside", "scratch", "target"] {
                    assert!(!report.err.contains(forbidden), "no path of the other side is printed: {}", report.err);
                }
            }
            assert!(kv_slots::ensure_slot_dir(&w.data).is_err(), "the saver refuses the same folder");
            assert_eq!(listing(&w.outside), outside_before);
            assert!(fs::symlink_metadata(w.kv()).is_ok(), "the link was not removed");
            // Take the link away (the link, not what it leads to).
            fs::remove_dir(w.kv()).unwrap();
            w.links.borrow_mut().clear();
        }
        // A link at `cache`, with a `kv` behind it.
        fs::remove_dir(w.data.join("cache")).unwrap();
        if w.dir_link(&w.data.join("cache"), &w.outside) {
            for report in [list(&w.data, NOW), forget(&w.data)] {
                assert_eq!((report.code, report.out.as_str()), (1, ""), "{report:?}");
                assert!(report.err.contains("link that leaves the data folder"), "{}", report.err);
            }
            assert!(kv_slots::ensure_slot_dir(&w.data).is_err());
            assert_eq!(listing(&w.outside), outside_before, "nothing outside was read, changed or deleted");
        }
    }

    #[test]
    fn a_link_inside_the_data_folder_is_followed_the_way_the_saver_follows_it() {
        let w = World::new("inside_link");
        let real = w.data.join("elsewhere");
        fs::create_dir_all(&real).unwrap();
        let name = slot_name(1, 2);
        put_slot(&real, &name, 64, NOW - 100);
        fs::create_dir_all(w.data.join("cache")).unwrap();
        if w.dir_link(&w.kv(), &real) {
            assert!(kv_slots::ensure_slot_dir(&w.data).is_ok());
            let report = list(&w.data, NOW);
            assert!(report.out.contains(&name) && report.out.starts_with("1 saved slot file"), "{}", report.out);
            assert_eq!(forget(&w.data).out.lines().next(), Some("deleted 1 saved slot file, 64 bytes (64 B) from cache/kv"));
            assert!(!real.join(&name).exists() && real.is_dir());
        }
    }

    #[test]
    fn a_link_inside_the_folder_with_the_name_of_a_saved_slot_is_never_followed_or_deleted() {
        let w = World::new("inner_links");
        let kv = w.make_kv();
        let real = slot_name(1, 2);
        put_slot(&kv, &real, 30, NOW);
        // What the links lead to: a file and a folder outside, which must stay as they are.
        let outside_file = w.outside.join("precious.txt");
        fs::write(&outside_file, MARKER).unwrap();
        put_slot(&w.outside, &slot_name(9, 9), 30, NOW);
        let outside_dir = w.outside.join("a_folder");
        fs::create_dir_all(&outside_dir).unwrap();
        put_slot(&outside_dir, &slot_name(8, 8), 30, NOW);
        let outside_before = listing(&w.outside);
        let mut links = 0;
        if w.file_link(&kv.join(slot_name(3, 4)), &outside_file) {
            links += 1;
        }
        if w.dir_link(&kv.join(slot_name(5, 6)), &outside_dir) {
            links += 1;
        }
        let listed = list(&w.data, NOW);
        assert!(listed.out.starts_with("1 saved slot file in cache/kv"), "{}", listed.out);
        assert!(links == 0 || listed.out.contains(&format!("left alone: {}", plural(links, "entry", "entries"))), "{}", listed.out);
        assert!(!listed.out.contains(&slot_name(3, 4)) && !listed.out.contains(&slot_name(5, 6)), "{}", listed.out);
        let done = forget(&w.data);
        assert_eq!((done.code, done.err.as_str()), (0, ""), "{done:?}");
        assert!(done.out.starts_with("deleted 1 saved slot file, 30 bytes"), "{}", done.out);
        assert!(!kv.join(&real).exists());
        assert_eq!(listing(&w.outside), outside_before, "what the links lead to is as it was");
        assert_eq!(fs::read_to_string(&outside_file).unwrap(), MARKER);
        // The links are still there: left alone, not deleted.
        for link in w.links.borrow().iter() {
            assert!(fs::symlink_metadata(link).is_ok(), "{} was removed", link.display());
        }
    }

    #[test]
    fn a_file_that_cannot_be_deleted_is_named_the_others_are_still_deleted_and_the_command_ends_non_zero() {
        let w = World::new("cannot_delete");
        let kv = w.make_kv();
        let names: Vec<String> = (1..=4).map(|i| slot_name(i, i)).collect();
        for n in &names {
            put_slot(&kv, n, 100, NOW);
        }
        // A deletion that refuses one name (what any system does for a file that is in use).
        let stuck = names[1].clone();
        let refuse = |path: &Path| -> std::io::Result<()> {
            if path.file_name().and_then(|n| n.to_str()) == Some(stuck.as_str()) {
                Err(std::io::Error::new(ErrorKind::PermissionDenied, "refused by the test"))
            } else {
                fs::remove_file(path)
            }
        };
        let report = forget_with(&w.data, &refuse);
        assert_eq!(report.code, 1, "{report:?}");
        assert!(report.out.starts_with("deleted 3 saved slot files, 300 bytes"), "{}", report.out);
        assert!(report.err.contains(&format!("slots forget: {stuck} could not be deleted: refused by the test")), "{}", report.err);
        assert!(report.err.contains("1 saved slot file stayed"), "{}", report.err);
        assert!(!report.err.contains(MARKER) && !report.err.contains("scratch") && !report.err.contains("target"), "{}", report.err);
        let left: Vec<String> = fs::read_dir(&kv).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left, vec![stuck.clone()], "the others were tried and deleted");
        // A file that is gone before its turn is nobody's failure.
        let vanish = |path: &Path| -> std::io::Result<()> { fs::remove_file(path).and_then(|()| Err(std::io::Error::from(ErrorKind::NotFound))) };
        let report = forget_with(&w.data, &vanish);
        assert_eq!((report.code, report.out.as_str()), (0, "no saved slots\n"), "{report:?}");
        assert!(report.err.is_empty());
    }

    #[test]
    fn a_read_only_file_is_deleted_or_named_the_way_this_computer_decides() {
        let w = World::new("read_only");
        let kv = w.make_kv();
        let marked = slot_name(1, 1);
        let plain_one = slot_name(2, 2);
        put_slot(&kv, &marked, 100, NOW);
        put_slot(&kv, &plain_one, 100, NOW);
        let set_read_only = |path: &Path| {
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(path, perms).unwrap();
        };
        // Does this computer refuse to delete a read-only file at all? (Rust's `remove_file` on Windows 10 and later deletes it; an older Windows refuses; Unix looks at the folder, not the file.)
        let probe = w.root.join("probe.tmp");
        fs::write(&probe, "x").unwrap();
        set_read_only(&probe);
        let refused_here = fs::remove_file(&probe).is_err();
        eprintln!("this computer {} a read-only file", if refused_here { "refuses to delete" } else { "deletes" });
        set_read_only(&kv.join(&marked));
        let report = forget(&w.data);
        if refused_here {
            // The file is named, the other one went, and the command ends non-zero.
            assert_eq!(report.code, 1, "{report:?}");
            assert!(report.out.starts_with("deleted 1 saved slot file, 100 bytes"), "{}", report.out);
            assert!(report.err.contains(&format!("slots forget: {marked} could not be deleted")), "{}", report.err);
            assert!(kv.join(&marked).exists() && !kv.join(&plain_one).exists());
        } else {
            // Nothing refuses it: both go, and the command does not pretend that something failed.
            assert_eq!((report.code, report.err.as_str()), (0, ""), "{report:?}");
            assert!(report.out.starts_with("deleted 2 saved slot files, 200 bytes"), "{}", report.out);
            assert!(!kv.join(&marked).exists() && !kv.join(&plain_one).exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_file_in_use_is_reported_by_name_and_goes_on_the_next_run() {
        use std::os::windows::fs::OpenOptionsExt;
        let w = World::new("in_use");
        let kv = w.make_kv();
        let (busy, free) = (slot_name(1, 1), slot_name(2, 2));
        put_slot(&kv, &busy, 100, NOW);
        put_slot(&kv, &free, 100, NOW);
        // The model server holding the file open without sharing it.
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(kv.join(&busy)).unwrap();
        let report = forget(&w.data);
        assert_eq!(report.code, 1, "{report:?}");
        assert!(report.err.contains(&format!("slots forget: {busy} could not be deleted")), "{}", report.err);
        assert!(report.out.starts_with("deleted 1 saved slot file, 100 bytes"), "{}", report.out);
        assert!(kv.join(&busy).exists() && !kv.join(&free).exists());
        drop(held);
        let report = forget(&w.data);
        assert_eq!((report.code, report.err.as_str()), (0, ""), "{report:?}");
        assert!(report.out.starts_with("deleted 1 saved slot file, 100 bytes"), "{}", report.out);
        assert!(!kv.join(&busy).exists());
    }

    #[test]
    fn sizes_and_ages_are_said_in_words_a_person_reads() {
        assert_eq!([human(0), human(1023), human(1024), human(1_536), human(220 * 1024 * 1024), human(3 * 1024 * 1024 * 1024)], ["0 B", "1023 B", "1.0 KB", "1.5 KB", "220.0 MB", "3.0 GB"]);
        assert_eq!([grouped(0), grouped(999), grouped(1_000), grouped(230_764_544)], ["0", "999", "1,000", "230,764,544"]);
        let at = |secs: u64| Some(UNIX_EPOCH + std::time::Duration::from_secs(secs));
        assert_eq!(age_text(at(NOW - 5), NOW), "5 seconds ago");
        assert_eq!(age_text(at(NOW - 600), NOW), "10 minutes ago");
        assert_eq!(age_text(at(NOW - 7_200), NOW), "2 hours ago");
        assert_eq!(age_text(at(NOW - 3 * 86_400), NOW), "3 days ago");
        assert_eq!(age_text(at(NOW + 500), NOW), "0 seconds ago", "a file from the future is not an error");
        assert_eq!(age_text(None, NOW), "age not known");
    }
}
