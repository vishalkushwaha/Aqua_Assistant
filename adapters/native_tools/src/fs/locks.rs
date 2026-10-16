//! One writer at a time for each file the file tools change (card OA6-01, ED-06).
//!
//! `fs.edit`, `fs.write`, `fs.patch`, `fs.move`, `fs.delete`, `fs.restore` and `fs.undo_run` read a file, work out the new content and write it. Two of them on one file (two runs, a
//! sub-agent and the person's own request, the API) used to read the same old content and the second write threw the first away. Here each call takes the files it is going to change
//! before it reads them and gives them back when it is done, so the second call reads what the first wrote.
//!
//! * The key of a file is its path as the path guard resolved it (links and junctions already followed), lower-cased on Windows, which has no case in its file names.
//! * A call that needs several files takes them all at once (it waits until none is held), so two calls that name the same files in another order cannot wait for each other.
//! * The same thread asking again for a file it holds is let through (a tool that calls another one does not wait for itself).
//! * Only the files held right now are remembered: the table is empty when nothing is being changed, however many files were.
//! * A wait has an end (ten seconds, less than the fifteen a file tool is given before the host stops waiting for it, so that a call the host gave up on never writes afterwards): a call that
//!   cannot get its files says so, instead of hanging a run.

use aqua_tool_host::{ErrorCode, ToolError};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// How long a call waits for the files it needs.
pub const WAIT: Duration = Duration::from_secs(10);

thread_local! {
    /// The keys this thread holds, so that asking for one again does not wait for itself.
    static MINE: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

#[derive(Default)]
pub struct PathLocks {
    held: Mutex<HashSet<String>>,
    freed: Condvar,
}

/// Files taken by [`PathLocks::lock_all`]; they are given back when this goes out of scope.
pub struct Held<'a> {
    locks: &'a PathLocks,
    keys: Vec<String>,
}

fn key_of(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PathLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes every file of `paths`, waiting up to `wait` for the ones another call holds. `Err(path)` names the file that was still held when the time ran out.
    pub fn lock_all(&self, paths: &[&Path], wait: Duration) -> Result<Held<'_>, String> {
        let mut keys: Vec<String> = paths.iter().map(|p| key_of(p)).collect();
        keys.sort();
        keys.dedup();
        // What this thread holds already is not asked for again.
        let ask: Vec<String> = MINE.with(|m| {
            let mine = m.borrow();
            keys.iter().filter(|k| !mine.contains(*k)).cloned().collect()
        });
        let deadline = Instant::now() + wait;
        let mut held = unpoisoned(&self.held);
        loop {
            if let Some(busy) = ask.iter().find(|k| held.contains(*k)) {
                let now = Instant::now();
                if now >= deadline {
                    return Err(busy.clone());
                }
                held = self.freed.wait_timeout(held, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
                continue;
            }
            for k in &ask {
                held.insert(k.clone());
            }
            break;
        }
        drop(held);
        MINE.with(|m| m.borrow_mut().extend(ask.iter().cloned()));
        Ok(Held { locks: self, keys: ask })
    }

    /// How many files are held now (for tests: it is zero when nothing is being changed).
    pub fn held_now(&self) -> usize {
        unpoisoned(&self.held).len()
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        MINE.with(|m| {
            let mut mine = m.borrow_mut();
            for k in &self.keys {
                mine.remove(k);
            }
        });
        let mut held = unpoisoned(&self.locks.held);
        for k in &self.keys {
            held.remove(k);
        }
        drop(held);
        self.locks.freed.notify_all();
    }
}

/// The locks of this process: every file tool of every provider in it shares them.
pub fn shared() -> &'static PathLocks {
    static LOCKS: OnceLock<PathLocks> = OnceLock::new();
    LOCKS.get_or_init(PathLocks::new)
}

/// Takes the files a tool call is about to change. When another call keeps one for more than [`WAIT`] the answer is a conflict the model can act on.
pub fn lock(paths: &[&Path]) -> Result<Held<'static>, ToolError> {
    lock_waiting(paths, WAIT)
}

pub fn lock_waiting(paths: &[&Path], wait: Duration) -> Result<Held<'static>, ToolError> {
    shared().lock_all(paths, wait).map_err(|busy| {
        let shown = paths.iter().find(|p| key_of(p) == busy).map(|p| super::guard::display(p)).unwrap_or(busy);
        ToolError::new(ErrorCode::Conflict, format!("{shown} is being changed by another call; wait a moment and read it again before editing"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_file_is_held_by_one_call_at_a_time_and_given_back() {
        let locks = Arc::new(PathLocks::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let overlap = Arc::new(AtomicUsize::new(0));
        let path = PathBuf::from("E:/ws/a.txt");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (locks, inside, overlap, path) = (locks.clone(), inside.clone(), overlap.clone(), path.clone());
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let _held = locks.lock_all(&[path.as_path()], Duration::from_secs(10)).unwrap();
                        if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlap.fetch_add(1, Ordering::SeqCst);
                        }
                        std::thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(overlap.load(Ordering::SeqCst), 0, "two calls were inside at once");
        assert_eq!(locks.held_now(), 0, "nothing stays in the table");
    }

    #[test]
    fn calls_that_name_the_same_files_in_another_order_do_not_wait_for_each_other() {
        let locks = Arc::new(PathLocks::new());
        let (a, b) = (PathBuf::from("E:/ws/a.txt"), PathBuf::from("E:/ws/b.txt"));
        let done = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..6)
            .map(|i| {
                let (locks, a, b, done) = (locks.clone(), a.clone(), b.clone(), done.clone());
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let order = if i % 2 == 0 { [a.as_path(), b.as_path()] } else { [b.as_path(), a.as_path()] };
                        let _held = locks.lock_all(&order, Duration::from_secs(10)).expect("no deadlock");
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(done.load(Ordering::SeqCst), 300);
        assert_eq!(locks.held_now(), 0);
    }

    #[test]
    fn the_thread_that_holds_a_file_may_ask_for_it_again_and_another_waits_then_gives_up() {
        let locks = Arc::new(PathLocks::new());
        let p = PathBuf::from("E:/ws/c.txt");
        let outer = locks.lock_all(&[p.as_path()], Duration::from_secs(1)).unwrap();
        {
            let _inner = locks.lock_all(&[p.as_path()], Duration::from_millis(10)).expect("the same thread is let through");
        }
        assert_eq!(locks.held_now(), 1, "the inner call gave back nothing it did not take");
        let other = {
            let (locks, p) = (locks.clone(), p.clone());
            std::thread::spawn(move || {
                let started = Instant::now();
                let r = locks.lock_all(&[p.as_path()], Duration::from_millis(150));
                (r.is_err(), started.elapsed())
            })
            .join()
            .unwrap()
        };
        assert!(other.0, "another thread cannot take it");
        assert!(other.1 >= Duration::from_millis(100), "it waited for the time it was given: {:?}", other.1);
        drop(outer);
        assert_eq!(locks.held_now(), 0);
        let _again = locks.lock_all(&[p.as_path()], Duration::from_millis(10)).unwrap();
    }

    #[test]
    fn a_name_is_the_same_file_whatever_its_case_or_slash_on_windows() {
        if !cfg!(windows) {
            return;
        }
        let locks = PathLocks::new();
        let held = locks.lock_all(&[Path::new("E:\\WS\\A.txt")], Duration::from_secs(1)).unwrap();
        let t = std::thread::scope(|s| s.spawn(|| locks.lock_all(&[Path::new("e:/ws/a.TXT")], Duration::from_millis(50)).is_err()).join().unwrap());
        assert!(t, "the same file under another spelling is the same lock");
        drop(held);
    }
}
