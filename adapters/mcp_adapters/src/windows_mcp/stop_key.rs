//! The Esc key as a stop while a run is acting on the screen (card R6-34).
//!
//! A run that clicks and types in the person's windows must be stoppable by the person, at once, without finding a window or a button: Esc, pressed on the keyboard, stops it. While
//! a desktop session is going ([`super::session`]) the key is **consumed**: the program in front never sees that press (so Esc does not also close the dialog the person wanted to
//! keep), the session ends, and the run is told to stop. When no session is going, Esc is nobody's but the program in front, and passes untouched.
//!
//! - **Only a real key.** Keys the agent presses itself arrive marked as injected by the system, and are never a stop: the agent cannot stop itself or another run with its own
//!   Esc, and a page or a program that sends keys cannot use the stop to spoof the person.
//! - **Once.** The press that stops is consumed with its release and its repeats; the next Esc is the program's.
//! - **It never holds a key.** The hook answers in microseconds: the stop itself runs on another thread.
//!
//! The decision is pure ([`StopKey::on_event`]) and tested; the hook is the thin layer that feeds it ([`install`], Windows only: a low-level keyboard hook on a thread of its own).

use std::sync::atomic::{AtomicBool, Ordering};

pub const VK_ESCAPE: u32 = 0x1B;
/// `LLKHF_INJECTED` and `LLKHF_LOWER_IL_INJECTED` of the low-level keyboard hook's flags: the key was sent by a program, not typed.
const INJECTED: u32 = 0x10 | 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Not ours: on to the program in front.
    Pass,
    /// The person pressed Esc during a session: stop it, and the program does not see the key.
    Stop,
    /// The rest of the press that stopped (its release, its repeats): the program does not see it either.
    Swallow,
}

/// What a key event means, with the one piece of memory it needs (that a press was consumed, so that its release is too).
#[derive(Debug)]
pub struct StopKey {
    consuming: AtomicBool,
}

impl Default for StopKey {
    fn default() -> Self {
        Self::new()
    }
}

impl StopKey {
    pub const fn new() -> Self {
        Self { consuming: AtomicBool::new(false) }
    }

    /// `vk` is the virtual key, `flags` the hook's flags, `is_down` a press (not a release), `live` whether a desktop session is going.
    pub fn on_event(&self, vk: u32, flags: u32, is_down: bool, live: bool) -> KeyAction {
        if vk != VK_ESCAPE || flags & INJECTED != 0 {
            return KeyAction::Pass;
        }
        if self.consuming.load(Ordering::SeqCst) {
            // The press that stopped is still down: its repeats and its release are consumed, and the release ends it.
            if !is_down {
                self.consuming.store(false, Ordering::SeqCst);
            }
            return KeyAction::Swallow;
        }
        if is_down && live {
            self.consuming.store(true, Ordering::SeqCst);
            return KeyAction::Stop;
        }
        KeyAction::Pass
    }
}

static KEY: StopKey = StopKey::new();

/// What a stop does, given by the program that owns the runs (the turn is told to stop, as the Stop button does it).
type OnStop = Box<dyn Fn() + Send + Sync>;
static ON_STOP: std::sync::OnceLock<OnStop> = std::sync::OnceLock::new();

/// A press of Esc as the hook sees it. The session is let go of first, so that the key is the program's again at once, then the stop runs on a thread of its own.
fn handle(vk: u32, flags: u32, is_down: bool) -> KeyAction {
    let action = KEY.on_event(vk, flags, is_down, super::session::global().is_live_at(std::time::Instant::now()));
    if action == KeyAction::Stop {
        super::session::global().release_all();
        tracing::info!("the Esc key was pressed during a desktop session: the session is stopped");
        if ON_STOP.get().is_some() {
            let _ = std::thread::Builder::new().name("aqua-esc-stop".into()).spawn(|| {
                if let Some(stop) = ON_STOP.get() {
                    stop();
                }
            });
        }
    }
    action
}

/// Installs the hook, once for the process: `on_stop` is what stops the runs. Returns an error where it cannot be done (not Windows, or the system refused the hook); the
/// product then runs without the key, and says so in the log.
#[cfg(windows)]
pub fn install(on_stop: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN};

    if ON_STOP.set(Box::new(on_stop)).is_err() {
        return Err("the Esc key hook is already installed".into());
    }

    unsafe extern "system" fn hook(code: i32, wparam: usize, lparam: isize) -> isize {
        use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, HC_ACTION};
        if code == HC_ACTION as i32 && lparam != 0 {
            // SAFETY: for a low-level keyboard hook, `lparam` points to a KBDLLHOOKSTRUCT that is valid for the duration of the call.
            let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let is_down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
            if handle(event.vkCode, event.flags, is_down) != KeyAction::Pass {
                return 1; // consumed: the program in front does not get the key
            }
        }
        // SAFETY: passes the event on exactly as it came.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("aqua-esc-hook".into())
        .spawn(move || {
            // SAFETY: the hook procedure above has the signature the system calls it with; the module handle is this program's.
            let installed = unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module, 0)
            };
            if installed.is_null() {
                let _ = tx.send(Err("the system did not accept the keyboard hook".to_string()));
                return;
            }
            let _ = tx.send(Ok(()));
            // A low-level hook is called through the message loop of the thread that installed it.
            // SAFETY: `msg` is a valid MSG for the call to fill; the loop ends only if the message queue fails.
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
            }
        })
        .map_err(|e| format!("the thread of the keyboard hook did not start: {e}"))?;
    rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "the keyboard hook did not answer".to_string())?
}

#[cfg(not(windows))]
pub fn install(_on_stop: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
    Err("the Esc key stop is for Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOWN: bool = true;
    const UP: bool = false;

    #[test]
    fn esc_during_a_session_stops_it_and_is_consumed_with_its_repeats_and_its_release_and_only_once() {
        let k = StopKey::new();
        assert_eq!(k.on_event(VK_ESCAPE, 0, DOWN, true), KeyAction::Stop);
        // Held down: the system repeats the press. The session is over by now (`live` is false), and the repeats are still not the program's.
        assert_eq!(k.on_event(VK_ESCAPE, 0, DOWN, false), KeyAction::Swallow);
        assert_eq!(k.on_event(VK_ESCAPE, 0, DOWN, false), KeyAction::Swallow);
        assert_eq!(k.on_event(VK_ESCAPE, 0, UP, false), KeyAction::Swallow, "the release of the press that stopped");
        // The next Esc is the program's, whether or not a session is going.
        assert_eq!(k.on_event(VK_ESCAPE, 0, DOWN, false), KeyAction::Pass);
        assert_eq!(k.on_event(VK_ESCAPE, 0, UP, false), KeyAction::Pass);
    }

    #[test]
    fn with_no_session_going_esc_and_every_other_key_belong_to_the_program_in_front() {
        let k = StopKey::new();
        for (vk, live) in [(VK_ESCAPE, false), (0x41, true), (0x0D, true), (0x20, true), (0x70, true)] {
            assert_eq!(k.on_event(vk, 0, DOWN, live), KeyAction::Pass, "key {vk:#x} live={live}");
            assert_eq!(k.on_event(vk, 0, UP, live), KeyAction::Pass, "key {vk:#x} live={live}");
        }
        // A release of Esc that was never a stop is not consumed.
        assert_eq!(k.on_event(VK_ESCAPE, 0, UP, true), KeyAction::Pass);
    }

    #[test]
    fn a_key_that_a_program_sent_is_never_a_stop_so_the_agent_cannot_stop_itself_and_nothing_can_spoof_the_person() {
        let k = StopKey::new();
        for flags in [0x10u32, 0x02, 0x12, 0x10 | 0x20 | 0x80] {
            assert_eq!(k.on_event(VK_ESCAPE, flags, DOWN, true), KeyAction::Pass, "flags {flags:#x}");
            assert_eq!(k.on_event(VK_ESCAPE, flags, UP, true), KeyAction::Pass, "flags {flags:#x}");
        }
        // The same press from the keyboard is.
        assert_eq!(k.on_event(VK_ESCAPE, 0x20, DOWN, true), KeyAction::Stop, "flags that are not the injected ones (ALT down) do not stop it being the person's");
    }

    #[test]
    fn a_stop_ends_the_session_and_runs_what_it_was_given_off_the_hooks_thread() {
        // The whole path of a press, as the hook feeds it: the lease is let go and the stop runs, once, on another thread.
        // The stop is one per process and this is the only test that sets it.
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let seen = ran.clone();
        assert!(ON_STOP.set(Box::new(move || seen.store(true, Ordering::SeqCst))).is_ok(), "no other test sets what a stop does");
        super::super::session::global().claim_at("run_stop_test", std::time::Instant::now()).unwrap();
        assert!(super::super::session::global().is_live_at(std::time::Instant::now()));
        assert_eq!(handle(VK_ESCAPE, 0, DOWN), KeyAction::Stop);
        assert!(!super::super::session::global().is_live_at(std::time::Instant::now()), "the session is let go of at once");
        assert_eq!(handle(VK_ESCAPE, 0, UP), KeyAction::Swallow);
        assert_eq!(handle(VK_ESCAPE, 0, DOWN), KeyAction::Pass, "no session: the next Esc is the program's");
        assert_eq!(handle(VK_ESCAPE, 0, UP), KeyAction::Pass);
        let started = std::time::Instant::now();
        while !ran.load(Ordering::SeqCst) && started.elapsed() < std::time::Duration::from_secs(3) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ran.load(Ordering::SeqCst), "the stop ran, on a thread of its own");
    }
}
