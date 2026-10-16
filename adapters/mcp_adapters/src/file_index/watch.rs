//! One thread watches every target with `ReadDirectoryChangesW` and tells the worker "something changed under here".
//!
//! It never touches the database and never interprets renames: a changed name just marks its folder for a re-list.
//! Only name changes (create, delete, rename) are requested, never content writes, so a file being saved does not wake
//! anything. Events inside folders that are never indexed are dropped here, before they cost the worker anything.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Where events go. Returns false when nobody is listening any more.
pub type Sink = Arc<dyn Fn(Event) -> bool + Send + Sync>;

/// Lower-case folder paths whose events are dropped here, before they reach the worker (Aqua's own data folder, the
/// user's excluded folders). Shared, so the worker can change it while the watcher runs.
pub type Blocked = Arc<std::sync::RwLock<Vec<String>>>;

fn is_blocked(full: &std::path::Path, blocked: &Blocked) -> bool {
    let Ok(list) = blocked.read() else { return false };
    if list.is_empty() {
        return false;
    }
    let p = full.to_string_lossy().to_lowercase();
    list.iter().any(|b| p == *b || p.starts_with(&format!("{b}\\")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Something was created, deleted or renamed at this full path.
    Changed(PathBuf),
    /// Windows dropped events for this target: its whole tree needs a re-list.
    Overflow(PathBuf),
    /// The target could not be watched any more (folder deleted or renamed, access lost).
    Lost(PathBuf),
    /// Far too many events for far too long: stop watching it and re-list it now and then instead.
    Stormed(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetState {
    pub path: PathBuf,
    pub state: &'static str,
}

/// The names in a `FILE_NOTIFY_INFORMATION` buffer: (action, relative name). Bounds-checked; a damaged buffer ends the
/// list instead of reading past it.
pub fn decode(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    let rd = |o: usize| -> Option<u32> { buf.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) };
    while off + 12 <= buf.len() {
        let (Some(next), Some(action), Some(nlen)) = (rd(off), rd(off + 4), rd(off + 8)) else { break };
        let nlen = nlen as usize;
        let Some(raw) = buf.get(off + 12..off + 12 + nlen) else { break };
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        out.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        off += next as usize;
    }
    out
}

pub struct Watcher {
    imp: imp::Handle,
    pub states: Arc<Mutex<Vec<TargetState>>>,
}

impl Watcher {
    pub fn start(events: Sink, blocked: Blocked) -> Watcher {
        let states = Arc::new(Mutex::new(Vec::new()));
        Watcher { imp: imp::start(events, states.clone(), blocked), states }
    }
    /// Replace the set of watched targets (at most 60 are used).
    pub fn set_targets(&self, targets: Vec<Target>) {
        self.imp.set(targets);
    }
    pub fn stop(self) {
        self.imp.stop();
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::file_index::platform::long_path;
    use crate::file_index::rules;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    const MAX_TARGETS: usize = 60;
    const BUF_BYTES: usize = 64 * 1024;
    /// More than this many events in a second, for this many seconds in a row, is a storm.
    const STORM_PER_SEC: u32 = 500;
    const STORM_SECONDS: u32 = 10;
    const ERROR_NOTIFY_ENUM_DIR: u32 = 1022;

    enum Ctl {
        Set(Vec<Target>),
        Stop,
    }

    pub struct Handle {
        ctl: Sender<Ctl>,
        wake: usize,
        join: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl Handle {
        pub fn set(&self, t: Vec<Target>) {
            let _ = self.ctl.send(Ctl::Set(t));
            // SAFETY: `wake` is an event handle that lives until the thread ends; SetEvent on it is always allowed.
            unsafe { SetEvent(self.wake as HANDLE) };
        }
        pub fn stop(&self) {
            let _ = self.ctl.send(Ctl::Stop);
            unsafe { SetEvent(self.wake as HANDLE) };
            if let Some(j) = self.join.lock().ok().and_then(|mut j| j.take()) {
                let _ = j.join();
            }
        }
    }

    struct Live {
        target: Target,
        handle: HANDLE,
        ov: OVERLAPPED,
        buf: Vec<u64>,
        window: Instant,
        count: u32,
        hot_seconds: u32,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn arm(l: &mut Live) -> bool {
        // SAFETY: the buffer and the OVERLAPPED live in a Box that is not moved or freed until the read has completed
        // or been cancelled and waited for (see `close`).
        unsafe {
            ReadDirectoryChangesW(
                l.handle,
                l.buf.as_mut_ptr() as *mut _,
                BUF_BYTES as u32,
                l.target.recursive as i32,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                std::ptr::null_mut(),
                &mut l.ov,
                None,
            ) != 0
        }
    }

    fn open(target: &Target) -> Option<Box<Live>> {
        // SAFETY: valid NUL-terminated path; the handle is closed in `close`.
        unsafe {
            let h = CreateFileW(
                wide(&long_path(&target.path).to_string_lossy()).as_ptr(),
                FILE_LIST_DIRECTORY,
                // Sharing delete and write matters: the user must still be able to rename and delete watched folders.
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                return None;
            }
            let ev = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            if ev.is_null() {
                CloseHandle(h);
                return None;
            }
            let mut l = Box::new(Live { target: target.clone(), handle: h, ov: std::mem::zeroed(), buf: vec![0u64; BUF_BYTES / 8], window: Instant::now(), count: 0, hot_seconds: 0 });
            l.ov.hEvent = ev;
            if !arm(&mut l) {
                close(l);
                return None;
            }
            Some(l)
        }
    }

    /// Cancel the pending read and WAIT for it to finish before the buffer is freed (it would otherwise be written to).
    fn close(l: Box<Live>) {
        // SAFETY: handle and event are ours; GetOverlappedResult with wait=true returns once the cancel is complete.
        unsafe {
            CancelIoEx(l.handle, &l.ov);
            let mut n = 0u32;
            GetOverlappedResult(l.handle, &l.ov, &mut n, 1);
            CloseHandle(l.handle);
            CloseHandle(l.ov.hEvent);
        }
    }

    pub fn start(events: Sink, states: Arc<Mutex<Vec<TargetState>>>, blocked: Blocked) -> Handle {
        let (ctl_tx, ctl_rx) = channel::<Ctl>();
        // SAFETY: plain event creation.
        let wake = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) } as usize;
        let join = std::thread::Builder::new()
            .name("aqua-index-watch".into())
            .spawn(move || run(wake, ctl_rx, events, states, blocked))
            .ok();
        Handle { ctl: ctl_tx, wake, join: Mutex::new(join) }
    }

    fn publish(states: &Arc<Mutex<Vec<TargetState>>>, live: &[Box<Live>], other: &[TargetState]) {
        if let Ok(mut s) = states.lock() {
            *s = live.iter().map(|l| TargetState { path: l.target.path.clone(), state: "watching" }).chain(other.iter().cloned()).collect();
        }
    }

    fn run(wake: usize, ctl: Receiver<Ctl>, sink: Sink, states: Arc<Mutex<Vec<TargetState>>>, blocked: Blocked) {
        let wake = wake as HANDLE;
        let mut live: Vec<Box<Live>> = Vec::new();
        let mut other: Vec<TargetState> = Vec::new(); // polled or lost
        loop {
            let mut handles: Vec<HANDLE> = Vec::with_capacity(live.len() + 1);
            handles.push(wake);
            handles.extend(live.iter().map(|l| l.ov.hEvent));
            // SAFETY: `handles` holds valid event handles for the duration of the call.
            let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 60_000) };
            if r == WAIT_TIMEOUT {
                continue;
            }
            let idx = r as usize; // WAIT_OBJECT_0 is 0
            if idx >= handles.len() {
                // Wait failed: back off instead of spinning.
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            if idx == 0 {
                // SAFETY: our own event.
                unsafe { ResetEvent(wake) };
                while let Ok(c) = ctl.try_recv() {
                    match c {
                        Ctl::Stop => {
                            for l in live.drain(..) {
                                close(l);
                            }
                            unsafe { CloseHandle(wake) };
                            return;
                        }
                        Ctl::Set(targets) => {
                            let targets: Vec<Target> = targets.into_iter().take(MAX_TARGETS).collect();
                            // Close the ones no longer wanted, open the new ones, keep the rest running.
                            let mut keep: Vec<Box<Live>> = Vec::new();
                            for l in live.drain(..) {
                                if targets.contains(&l.target) {
                                    keep.push(l);
                                } else {
                                    close(l);
                                }
                            }
                            other.clear();
                            for t in targets {
                                if keep.iter().any(|l| l.target == t) {
                                    continue;
                                }
                                match open(&t) {
                                    Some(l) => keep.push(l),
                                    None => other.push(TargetState { path: t.path.clone(), state: "lost" }),
                                }
                            }
                            live = keep;
                            publish(&states, &live, &other);
                        }
                    }
                }
                continue;
            }
            let i = idx - 1;
            let mut n = 0u32;
            // SAFETY: handle and OVERLAPPED are ours and the operation has completed (its event is signalled).
            let ok = unsafe { GetOverlappedResult(live[i].handle, &live[i].ov, &mut n, 0) };
            let path = live[i].target.path.clone();
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_NOTIFY_ENUM_DIR {
                    let _ = sink(Event::Overflow(path));
                    if !arm(&mut live[i]) {
                        let l = live.remove(i);
                        other.push(TargetState { path: l.target.path.clone(), state: "lost" });
                        let _ = sink(Event::Lost(l.target.path.clone()));
                        close(l);
                        publish(&states, &live, &other);
                    }
                } else {
                    // Access denied, the folder was deleted or renamed, the handle is no longer valid.
                    let l = live.remove(i);
                    other.push(TargetState { path: l.target.path.clone(), state: "lost" });
                    let _ = sink(Event::Lost(l.target.path.clone()));
                    close(l);
                    publish(&states, &live, &other);
                }
                continue;
            }
            if n == 0 {
                // A completion with no data: Windows dropped events.
                let _ = sink(Event::Overflow(path));
                if !arm(&mut live[i]) {
                    let l = live.remove(i);
                    other.push(TargetState { path: l.target.path.clone(), state: "lost" });
                    let _ = sink(Event::Lost(l.target.path.clone()));
                    close(l);
                    publish(&states, &live, &other);
                }
                continue;
            }
            // Copy the data out and start the next read first, so events are not lost while parsing.
            let bytes: Vec<u8> = {
                // SAFETY: n <= BUF_BYTES bytes of the u64 buffer were written by the kernel.
                let p = live[i].buf.as_ptr() as *const u8;
                unsafe { std::slice::from_raw_parts(p, (n as usize).min(BUF_BYTES)).to_vec() }
            };
            let rearmed = arm(&mut live[i]);
            let mut count = 0u32;
            for (_action, name) in decode(&bytes) {
                if rules::path_in_noise(name.split(['\\', '/'])) {
                    continue;
                }
                let full = path.join(&name);
                if is_blocked(&full, &blocked) {
                    continue;
                }
                count += 1;
                if !sink(Event::Changed(full)) {
                    for l in live.drain(..) {
                        close(l);
                    }
                    return;
                }
            }
            // Storm valve.
            let l = &mut live[i];
            l.count += count;
            if l.window.elapsed() >= Duration::from_secs(1) {
                l.hot_seconds = if l.count > STORM_PER_SEC { l.hot_seconds + 1 } else { 0 };
                l.count = 0;
                l.window = Instant::now();
            }
            if !rearmed || l.hot_seconds >= STORM_SECONDS {
                let storm = rearmed;
                let l = live.remove(i);
                other.push(TargetState { path: l.target.path.clone(), state: if storm { "polled" } else { "lost" } });
                let _ = sink(if storm { Event::Stormed(l.target.path.clone()) } else { Event::Lost(l.target.path.clone()) });
                close(l);
                publish(&states, &live, &other);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub struct Handle;
    impl Handle {
        pub fn set(&self, _t: Vec<Target>) {}
        pub fn stop(&self) {}
    }
    pub fn start(_events: Sink, _states: Arc<Mutex<Vec<TargetState>>>, _blocked: Blocked) -> Handle {
        Handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend(next.to_le_bytes());
        v.extend(action.to_le_bytes());
        v.extend(((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend(u.to_le_bytes());
        }
        // entries are 4-byte aligned
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn the_notification_buffer_is_decoded_in_order_including_odd_names() {
        let a = entry(0, 4, "old.txt"); // length known after padding
        let mut first = entry(0, 4, "old.txt");
        let next = first.len() as u32;
        first[0..4].copy_from_slice(&next.to_le_bytes());
        let mut buf = first;
        buf.extend(entry(0, 5, "new 名前.txt"));
        let got = decode(&buf);
        assert_eq!(got, vec![(4, "old.txt".to_string()), (5, "new 名前.txt".to_string())]);
        assert!(!a.is_empty());
    }

    #[test]
    fn a_damaged_buffer_ends_the_list_instead_of_reading_past_it() {
        let mut buf = entry(0, 1, "a.txt");
        buf.truncate(buf.len() - 6); // name length now longer than the data
        assert!(decode(&buf).is_empty());
        assert!(decode(&[1, 2, 3]).is_empty());
        // a NextEntryOffset that points past the end stops after the first entry
        let mut one = entry(0, 1, "x");
        one[0..4].copy_from_slice(&9999u32.to_le_bytes());
        assert_eq!(decode(&one).len(), 1);
    }
}
