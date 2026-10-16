//! Open a web URL in the user's default browser without going through a shell.
//!
//! `cmd /C start` treats `&`, `^` and `%` in a URL as command syntax, which lets a crafted URL
//! run a second command; PowerShell needed quoting that the agent could not be trusted with.
//! `ShellExecuteW` takes the URL as data.

use anyhow::{bail, Result};

/// Only plain http(s) URLs may be handed to the shell; anything else (paths, `file:`, `ms-*:`
/// handlers) would let model output launch arbitrary programs.
pub fn is_safe_web_url(url: &str) -> bool {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("https://") {
        r
    } else if let Some(r) = lower.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty()
        && u.len() <= 2048
        && !u.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '`')
}

pub fn open_web_url(url: &str) -> Result<()> {
    if !is_safe_web_url(url) {
        bail!("refusing to open non-http(s) target: {url}");
    }
    let started = std::time::Instant::now();
    let result = open_impl(url.trim());
    // Opening a page goes round the tool host, so it writes its own line in the same ledger. A query string can hold a secret: only
    // the address up to it is recorded.
    let shown: String = url.trim().split(['?', '#']).next().unwrap_or("").chars().take(200).collect();
    aqua_tool_host::record_direct("direct", "shell.open_url", &shown, result.is_ok(), started.elapsed().as_millis() as u64);
    result
}

/// Open a file or folder the way double-clicking it would, in its own folder. The caller decides what may be opened
/// (see `local_files`); the path goes to the shell as data, never through a command line.
pub fn open_local_path(path: &std::path::Path) -> Result<()> {
    let started = std::time::Instant::now();
    let result = open_local_path_inner(path);
    aqua_tool_host::record_direct("direct", "shell.open_path", &path.to_string_lossy(), result.is_ok(), started.elapsed().as_millis() as u64);
    result
}

fn open_local_path_inner(path: &std::path::Path) -> Result<()> {
    let target = path.to_string_lossy();
    if target.chars().any(|c| c.is_control()) {
        bail!("refusing a path with control characters");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let verb = wide("open");
        let file = wide(&target);
        let dir = path.parent().map(|p| wide(&p.to_string_lossy()));
        // SAFETY: every string is NUL-terminated and outlives the call.
        let rc = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                SW_SHOWNORMAL,
            )
        } as isize;
        if rc <= 32 {
            bail!("Windows could not open it (ShellExecuteW code {rc})");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        open_impl(&target)
    }
}

#[cfg(windows)]
fn open_impl(url: &str) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = wide("open");
    let target = wide(url);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if rc <= 32 {
        bail!("ShellExecuteW failed with code {rc}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_impl(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    std::process::Command::new(opener).arg(url).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_web_urls_and_rejects_everything_else() {
        assert!(is_safe_web_url("https://example.com/a?b=1&c=2"));
        assert!(!is_safe_web_url("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_safe_web_url("ms-settings:privacy"));
        assert!(!is_safe_web_url("https://example.com/ x"));
        assert!(!is_safe_web_url("https://"));
        assert!(!is_safe_web_url("calc.exe"));
    }
}
