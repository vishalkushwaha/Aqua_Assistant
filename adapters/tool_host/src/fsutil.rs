//! Durable state files (card OA3-02; task QA-13 of the OpenCode audit).
//!
//! About twenty places in Aqua wrote a state file with the same ten lines: write a temporary file, rename it over the target. Almost none of them
//! flushed the temporary file first, so after a power cut NTFS could show a zero-length target after the rename; several wrote a name that two
//! writers shared; a few skipped the temporary file altogether (`std::fs::write` on the trust record, which a crash leaves truncated). This module
//! is the one tested copy.
//!
//! * [`write_atomic_durable`] writes to a new file next to the target (`create_new`, so a link planted at the name is never followed), flushes it
//!   (`sync_all`), and replaces the target. On Windows the replace is `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`, which does
//!   not return before the rename is on disk; elsewhere the folder is flushed too. On an error the temporary file is removed and the old target is
//!   untouched. A target on another drive is an error (there is no copy-then-delete fallback: that is not atomic).
//! * [`read_json_strict`] tells *absent* (`Ok(None)`) from *unreadable* (a lock, a permission) from *corrupt* (not valid for the type). The old
//!   readers turned all three into "nothing there" and the next write then replaced the file, which lost whatever it held.
//! * [`quarantine`] moves a file aside as `<name>.bad-<unix ms>` instead of deleting it.
//! * [`sweep_tmp`] removes the temporary files a killed process left behind (only names this module or `fs.write` makes, and only old ones).
//!
//! Std only: no new dependency. The one system call std does not offer is declared here.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The marker in the name of a temporary file: `.<name>.aqua-tmp-<pid>-<n>` (this module) or `<name>.aqua-tmp-<pid>-<n>` (`fs.write`).
pub const TMP_MARK: &str = ".aqua-tmp-";

/// Is this a symbolic link, or on Windows any reparse point (a junction is one)? The name of such a thing can lead somewhere else, so a reader of files that someone else can put
/// into a folder (a repository's `.aqua` pack, a skill's folder) passes over it. Take `meta` from `symlink_metadata`, which does not follow the link.
pub fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    let reparse = std::os::windows::fs::MetadataExt::file_attributes(meta) & 0x400 != 0;
    #[cfg(not(windows))]
    let reparse = false;
    meta.file_type().is_symlink() || reparse
}

/// Why a state file could not be read (see [`read_json_strict`]).
#[derive(Debug)]
pub enum ReadError {
    /// The file is there and could not be read: a lock, a permission, a device error. Its content is not known, so it must not be replaced.
    Unreadable(io::Error),
    /// The file was read and is not valid for the type asked for. Its content is whatever the bytes are; keep it (see [`quarantine`]).
    Corrupt(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Unreadable(e) => write!(f, "could not be read: {e}"),
            ReadError::Corrupt(e) => write!(f, "is damaged: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// The temporary name next to `target`: `.<name>.aqua-tmp-<pid>-<n>`.
fn temp_name_for(target: &Path, n: u64) -> PathBuf {
    let name = target.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "state".into());
    target.with_file_name(format!(".{name}{TMP_MARK}{}-{n}", std::process::id()))
}

/// Replaces `path` with `bytes` so that a reader sees the old file or the whole new one, never half of it, and so that the new one is on disk when
/// this returns. The folder is made when it is missing.
pub fn write_atomic_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_durable_with(path, bytes, |_| Ok(()))
}

/// [`write_atomic_durable`], with a step that runs on the temporary file after it is written and before it replaces the target. A token file uses it
/// to restrict who can open the file *before* the name exists, not after.
pub fn write_atomic_durable_with(path: &Path, bytes: &[u8], before_replace: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    for _ in 0..32 {
        let tmp = temp_name_for(path, COUNTER.fetch_add(1, Ordering::Relaxed));
        let mut f = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = (|| {
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            before_replace(&tmp)?;
            replace_durable(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return result;
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name for the temporary file"))
}

/// Renames `from` over `to` (replacing it) and does not return before the rename is on disk. A target that another program has open is tried three
/// times, 50 ms apart; any other error is returned at once. Used by [`write_atomic_durable`] and by `fs.write`, which makes its own temporary file
/// (it checks the folder first).
pub fn replace_durable(from: &Path, to: &Path) -> io::Result<()> {
    for attempt in 0..3 {
        match replace_once(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e) => {
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Err(e) => return Err(e),
        }
    }
    // Still busy: the plain rename of the standard library (on Windows 10 and later it replaces a file that another program has open with delete sharing, which the
    // write-through move above does not). It is as atomic and a little less sure to be on disk when it returns, and a target that cannot be replaced at all is an error.
    std::fs::rename(from, to)
}

/// A sharing or lock violation (another program has the file open), as opposed to a permission or a missing folder.
fn is_busy(e: &io::Error) -> bool {
    // 32 ERROR_SHARING_VIOLATION, 33 ERROR_LOCK_VIOLATION. Access denied (5) is also what an open handle without delete sharing gives on Windows.
    cfg!(windows) && matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33))
}

#[cfg(windows)]
fn replace_once(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let wide = |p: &Path| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (a, b) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated UTF-16 that live for the call; the flags are the documented ones.
    let ok = unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_once(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)?;
    // The rename is only durable once the folder that holds it is flushed.
    if let Some(dir) = to.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Reads a JSON state file. `Ok(None)` when the file is not there; [`ReadError::Unreadable`] when it is there and cannot be read;
/// [`ReadError::Corrupt`] when it is read and is not valid JSON for `T`. A leading byte order mark is ignored.
pub fn read_json_strict<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, ReadError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ReadError::Unreadable(e)),
    };
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice(body).map(Some).map_err(|e| ReadError::Corrupt(e.to_string()))
}

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Moves `path` aside as `<name>.bad-<unix ms>` (a counter is added when that name is taken) and returns the new path. The file keeps its content and
/// its permissions; nothing is deleted.
pub fn quarantine(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let stamp = unix_ms();
    for n in 0..1000u32 {
        let aside = path.with_file_name(if n == 0 { format!("{name}.bad-{stamp}") } else { format!("{name}.bad-{stamp}-{n}") });
        if aside.exists() {
            continue;
        }
        std::fs::rename(path, &aside)?;
        return Ok(aside);
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name to move the file to"))
}

/// Is this file name one of the temporary names this module or `fs.write` makes: `...aqua-tmp-<digits>-<digits>`?
pub fn is_tmp_name(name: &str) -> bool {
    let Some(at) = name.rfind(TMP_MARK) else { return false };
    if at == 0 && !name.starts_with('.') {
        return false;
    }
    let tail = &name[at + TMP_MARK.len()..];
    let Some((pid, n)) = tail.split_once('-') else { return false };
    !pid.is_empty() && !n.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && n.bytes().all(|b| b.is_ascii_digit())
}

/// Removes the temporary files in `dir` (this folder only) that are older than `older_than` and whose names are exactly ours; returns how many went.
/// A link is never followed and never removed here; a folder is left alone.
pub fn sweep_tmp(dir: &Path, older_than: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_tmp_name(&name) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.is_file() {
            continue;
        }
        let old_enough = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= older_than);
        if old_enough && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_fsutil_{name}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn files(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_no_temporary_file() {
        let d = world("replace");
        let p = d.join("state.json");
        write_atomic_durable(&p, b"first").unwrap();
        write_atomic_durable(&p, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second, longer");
        assert_eq!(files(&d), vec!["state.json".to_string()], "no temporary file is left");
        // A folder that does not exist is made.
        let deep = d.join("a").join("b").join("x.json");
        write_atomic_durable(&deep, b"{}").unwrap();
        assert_eq!(std::fs::read(&deep).unwrap(), b"{}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_replace_removes_the_temporary_file_and_keeps_the_old_target() {
        let d = world("fail");
        let p = d.join("state.json");
        write_atomic_durable(&p, b"old").unwrap();
        // The step before the replace refuses: the target stays as it was and the temporary file is gone.
        let err = write_atomic_durable_with(&p, b"new", |_| Err(io::Error::other("refused"))).unwrap_err();
        assert_eq!(err.to_string(), "refused");
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
        assert_eq!(files(&d), vec!["state.json".to_string()]);
        // A target that is a folder cannot be replaced by a file: an error, the temporary file removed, the folder untouched.
        let dir_target = d.join("folder");
        std::fs::create_dir(&dir_target).unwrap();
        assert!(write_atomic_durable(&dir_target, b"x").is_err());
        assert!(dir_target.is_dir());
        assert_eq!(files(&d), vec!["folder".to_string(), "state.json".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_step_before_the_replace_sees_the_new_content_under_a_temporary_name() {
        let d = world("before");
        let p = d.join("token");
        let seen = std::cell::RefCell::new(String::new());
        write_atomic_durable_with(&p, b"secret", |tmp| {
            *seen.borrow_mut() = format!("{}|{}", tmp.file_name().unwrap().to_string_lossy(), std::fs::read_to_string(tmp)?);
            Ok(())
        })
        .unwrap();
        let seen = seen.into_inner();
        assert!(seen.starts_with(".token.aqua-tmp-") && seen.ends_with("|secret"), "{seen}");
        assert!(is_tmp_name(seen.split('|').next().unwrap()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_writers_at_once_leave_one_whole_file() {
        let d = world("race");
        let p = d.join("state.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        let body = format!("{{\"writer\":{i},\"round\":{j},\"pad\":\"{}\"}}", "x".repeat(2000));
                        // On Windows a reader that holds the file open for an instant can make a replace fail; the helper retries, and a failure that remains is a
                        // legitimate error, but the file must never be half written.
                        let _ = write_atomic_durable(&p, body.as_bytes());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).expect("the file is one whole write");
        assert_eq!(v["pad"].as_str().unwrap().len(), 2000);
        assert!(files(&d).iter().all(|f| f == "state.json"), "no temporary file is left: {:?}", files(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_strict_read_tells_absent_from_unreadable_from_corrupt() {
        let d = world("strict");
        let p = d.join("list.json");
        assert!(matches!(read_json_strict::<Vec<u32>>(&p), Ok(None)), "absent");
        std::fs::write(&p, b"[1, 2").unwrap();
        assert!(matches!(read_json_strict::<Vec<u32>>(&p), Err(ReadError::Corrupt(_))), "truncated");
        std::fs::write(&p, b"{\"a\": 1}").unwrap();
        assert!(matches!(read_json_strict::<Vec<u32>>(&p), Err(ReadError::Corrupt(_))), "valid JSON of another shape");
        std::fs::write(&p, b"\xEF\xBB\xBF[1, 2, 3]").unwrap();
        assert_eq!(read_json_strict::<Vec<u32>>(&p).unwrap(), Some(vec![1, 2, 3]), "a byte order mark is ignored");
        // A folder where the file should be: it is there and cannot be read.
        let as_dir = d.join("dir.json");
        std::fs::create_dir(&as_dir).unwrap();
        assert!(matches!(read_json_strict::<Vec<u32>>(&as_dir), Err(ReadError::Unreadable(_))), "a folder is unreadable, not absent");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_quarantined_file_keeps_its_content_under_a_new_name() {
        let d = world("quarantine");
        let p = d.join("keys.json");
        std::fs::write(&p, b"broken").unwrap();
        let aside = quarantine(&p).unwrap();
        assert!(!p.exists());
        assert_eq!(std::fs::read(&aside).unwrap(), b"broken");
        let name = aside.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("keys.json.bad-") && name["keys.json.bad-".len()..].bytes().all(|b| b.is_ascii_digit()), "{name}");
        // A second one in the same millisecond gets a different name.
        std::fs::write(&p, b"broken again").unwrap();
        let second = quarantine(&p).unwrap();
        assert_ne!(aside, second);
        assert_eq!(std::fs::read(&second).unwrap(), b"broken again");
        assert!(quarantine(&d.join("missing.json")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_old_temporary_files_with_our_exact_names_are_swept() {
        let d = world("sweep");
        for keep in ["state.json", "notes.aqua-tmp-notes.txt", "x.aqua-tmp-12-", "x.aqua-tmp--3", "x.aqua-tmp-a-3", "aqua-tmp-1-2", ".aqua-tmp-1"] {
            std::fs::write(d.join(keep), b"k").unwrap();
        }
        for drop in [".state.json.aqua-tmp-4242-0", "report.aqua-tmp-4242-7", ".a.b.aqua-tmp-1-23"] {
            std::fs::write(d.join(drop), b"d").unwrap();
        }
        std::fs::create_dir(d.join(".dir.aqua-tmp-1-1")).unwrap();
        // Nothing is old enough yet.
        assert_eq!(sweep_tmp(&d, Duration::from_secs(3600)), 0);
        // With no age limit the three of ours go; the look-alikes and the folder stay.
        assert_eq!(sweep_tmp(&d, Duration::ZERO), 3);
        let left = files(&d);
        assert!(left.contains(&"state.json".to_string()) && left.contains(&".dir.aqua-tmp-1-1".to_string()) && left.contains(&"notes.aqua-tmp-notes.txt".to_string()), "{left:?}");
        assert!(!left.iter().any(|f| f == ".state.json.aqua-tmp-4242-0" || f == "report.aqua-tmp-4242-7"), "{left:?}");
        assert_eq!(sweep_tmp(&d.join("not-there"), Duration::ZERO), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn arbitrary_bytes_round_trip() {
        let d = world("bytes");
        let p = d.join("blob");
        // A cheap property test: a spread of lengths and byte patterns, including empty and non-UTF-8.
        let mut seed = 0x9E3779B97F4A7C15u64;
        for len in [0usize, 1, 2, 7, 255, 256, 4096, 65_537] {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            write_atomic_durable(&p, &bytes).unwrap();
            assert_eq!(std::fs::read(&p).unwrap(), bytes, "length {len}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
