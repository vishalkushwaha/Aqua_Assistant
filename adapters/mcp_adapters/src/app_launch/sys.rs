//! The Windows side of launching: which windows are open, starting something the way double-clicking it would, and reading the
//! Start menu once so a Store app's identifier can be learned. Each function is small and fails quietly (an empty list or an
//! Err the caller turns into "that way did not work").

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Win {
    pub hwnd: isize,
    pub title: String,
    /// The program's file name, lower case ("notepad.exe"); empty if Windows would not say.
    pub exe: String,
}

#[cfg(windows)]
pub use win::*;
#[cfg(not(windows))]
pub use other::*;

#[cfg(windows)]
mod win {
    use super::*;
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, SW_SHOWNORMAL, WS_EX_TOOLWINDOW};

    fn exe_of(pid: u32) -> String {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return String::new();
            }
            let mut buf = [0u16; 520];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            if ok == 0 {
                return String::new();
            }
            String::from_utf16_lossy(&buf[..len as usize]).replace('\\', "/").rsplit('/').next().unwrap_or("").to_lowercase()
        }
    }

    unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> BOOL {
        let list = &mut *(lp as *mut Vec<Win>);
        if IsWindowVisible(h) == 0 || (GetWindowLongW(h, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        let mut buf = [0u16; 300];
        let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, &mut pid);
        list.push(Win { hwnd: h as isize, title: String::from_utf16_lossy(&buf[..n as usize]), exe: exe_of(pid) });
        1
    }

    /// The visible, titled top-level windows now.
    pub fn visible_windows() -> Vec<Win> {
        let mut list: Vec<Win> = Vec::new();
        unsafe {
            EnumWindows(Some(collect), &mut list as *mut Vec<Win> as LPARAM);
        }
        list
    }

    /// Start a program, shortcut, Start menu entry or settings page, as double-clicking it would. The text goes to the shell as
    /// data (no command line, no arguments), and the caller has already checked it is something that may be started.
    pub fn shell_execute(target: &str) -> Result<(), String> {
        if target.chars().any(|c| c.is_control()) {
            return Err("the target has control characters".into());
        }
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let (verb, file) = (wide("open"), wide(target));
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let rc = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) } as isize;
        if rc <= 32 {
            return Err(format!("Windows could not start it (code {rc})"));
        }
        Ok(())
    }

    /// `Get-StartApps` as CSV. Takes a second or two (it starts PowerShell), so it is only used once per app, in the background.
    pub fn start_apps_csv(timeout: Duration) -> Option<String> {
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Get-StartApps | ConvertTo-Csv -NoTypeInformation"])
            .creation_flags(0x0800_0000) // no console window
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = out.read_to_end(&mut s);
            String::from_utf8_lossy(&s).into_owned()
        });
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(100)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        reader.join().ok().filter(|s| !s.trim().is_empty())
    }
}

#[cfg(not(windows))]
mod other {
    use super::*;
    pub fn visible_windows() -> Vec<Win> {
        Vec::new()
    }
    pub fn shell_execute(_target: &str) -> Result<(), String> {
        Err("starting apps is only supported on Windows".into())
    }
    pub fn start_apps_csv(_timeout: Duration) -> Option<String> {
        None
    }
}
