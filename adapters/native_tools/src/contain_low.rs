//! Running a program at **Low integrity** with a stripped token (Windows), for `term.run` with
//! `isolated: true`.
//!
//! Windows will not let a Low-integrity process write to anything labelled Medium or higher, and
//! nearly everything you own is Medium. So the child can read what your account can read, but it
//! can write only inside folders that were deliberately labelled Low (the per-run scratch folder
//! Aqua makes), it cannot change the registry's Medium keys, cannot change your files, and has
//! every removable privilege stripped from its token. It also cannot escape the Job Object it is
//! placed in before it runs a single instruction (it is started suspended).
//!
//! What this does **not** do: hide files from it (reads are allowed) or block its network access.
//! Do not put secrets where your account can read them and then run untrusted code isolated.

#![cfg(windows)]

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
    CreateRestrictedToken, SetTokenInformation, TokenIntegrityLevel, DISABLE_MAX_PRIVILEGE, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

const STILL_ACTIVE: u32 = 259;
/// `SE_GROUP_INTEGRITY`: the attribute of an integrity-level SID in a token.
const SE_GROUP_INTEGRITY: u32 = 0x20;

/// A child running at Low integrity. Closing it does not kill it; the Job Object does that.
pub struct LowChild {
    process: HANDLE,
    thread: HANDLE,
    pub pid: u32,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    resumed: bool,
}

// SAFETY: the handles are plain kernel handles owned by this struct and used from one thread at a time.
unsafe impl Send for LowChild {}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Quote one argument the way `CommandLineToArgvW` and the C runtime read it back.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"')) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<&(String, String)> = env.iter().collect();
    sorted.sort_by(|a, b| a.0.to_ascii_uppercase().cmp(&b.0.to_ascii_uppercase()));
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in sorted {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    block
}

/// A pipe whose `child_end` is inheritable and whose `parent_end` is not.
fn pipe(child_reads: bool) -> Result<(File, HANDLE), String> {
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid out-pointers; handles are closed or wrapped below.
    unsafe {
        if CreatePipe(&mut read, &mut write, &sa, 0) == 0 {
            return Err(format!("CreatePipe: {}", std::io::Error::last_os_error()));
        }
        let (parent, child) = if child_reads { (write, read) } else { (read, write) };
        SetHandleInformation(parent, HANDLE_FLAG_INHERIT, 0);
        Ok((File::from_raw_handle(parent as _), child))
    }
}

/// A primary token for this user with every removable privilege gone and the Low integrity label.
fn low_token() -> Result<HANDLE, String> {
    // SAFETY: standard token calls; every handle and the SID are released on all paths.
    unsafe {
        let mut current: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, &mut current) == 0 {
            return Err(format!("OpenProcessToken: {}", std::io::Error::last_os_error()));
        }
        let mut restricted: HANDLE = std::ptr::null_mut();
        let ok = CreateRestrictedToken(current, DISABLE_MAX_PRIVILEGE, 0, std::ptr::null(), 0, std::ptr::null(), 0, std::ptr::null(), &mut restricted);
        CloseHandle(current);
        if ok == 0 {
            return Err(format!("CreateRestrictedToken: {}", std::io::Error::last_os_error()));
        }
        let mut sid: *mut core::ffi::c_void = std::ptr::null_mut();
        let sddl = wide(OsStr::new("S-1-16-4096")); // Low mandatory level
        if ConvertStringSidToSidW(sddl.as_ptr(), &mut sid) == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(restricted);
            return Err(format!("ConvertStringSidToSidW: {e}"));
        }
        let label = TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: sid, Attributes: SE_GROUP_INTEGRITY } };
        let set = SetTokenInformation(restricted, TokenIntegrityLevel, &label as *const _ as *const core::ffi::c_void, std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32);
        let err = std::io::Error::last_os_error();
        LocalFree(sid);
        if set == 0 {
            CloseHandle(restricted);
            return Err(format!("SetTokenInformation(integrity): {err}"));
        }
        Ok(restricted)
    }
}

impl LowChild {
    /// Start `exe` suspended. Call [`LowChild::resume`] after it has been put in a Job Object.
    pub fn spawn(exe: &Path, args: &[String], cwd: &Path, env: &[(String, String)], wants_stdin: bool) -> Result<Self, String> {
        let token = low_token()?;
        let mut cmdline = quote_arg(&exe.to_string_lossy());
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(&quote_arg(a));
        }
        let mut cmd_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let exe_w = wide(exe.as_os_str());
        let cwd_w = wide(cwd.as_os_str());
        let env_w = env_block(env);

        let (stdout, out_child) = pipe(false).inspect_err(|_| unsafe {
            CloseHandle(token);
        })?;
        let (stderr, err_child) = pipe(false)?;
        let (stdin, in_child) = pipe(true)?;
        let stdin = if wants_stdin {
            Some(stdin)
        } else {
            drop(stdin); // the child sees end-of-file at once
            None
        };

        // SAFETY: all pointers outlive the call; handles are closed below.
        let (created, info) = unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.dwFlags = STARTF_USESTDHANDLES;
            si.hStdInput = in_child;
            si.hStdOutput = out_child;
            si.hStdError = err_child;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessAsUserW(
                token,
                exe_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                env_w.as_ptr() as *const core::ffi::c_void,
                cwd_w.as_ptr(),
                &si,
                &mut pi,
            );
            let e = std::io::Error::last_os_error();
            CloseHandle(token);
            CloseHandle(in_child);
            CloseHandle(out_child);
            CloseHandle(err_child);
            (if ok == 0 { Err(format!("CreateProcessAsUserW: {e}")) } else { Ok(()) }, pi)
        };
        created?;
        Ok(Self { process: info.hProcess, thread: info.hThread, pid: info.dwProcessId, stdin, stdout: Some(stdout), stderr: Some(stderr), resumed: false })
    }

    pub fn raw_process(&self) -> HANDLE {
        self.process
    }

    pub fn resume(&mut self) -> Result<(), String> {
        // SAFETY: the thread handle is valid until Drop.
        let r = unsafe { ResumeThread(self.thread) };
        if r == u32::MAX {
            return Err(format!("ResumeThread: {}", std::io::Error::last_os_error()));
        }
        self.resumed = true;
        Ok(())
    }

    /// `Some(code)` once it has exited.
    pub fn try_wait(&self) -> Option<i32> {
        // SAFETY: valid process handle.
        unsafe {
            if WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.process, &mut code);
            let _ = STILL_ACTIVE;
            Some(code as i32)
        }
    }

    pub fn kill(&self) {
        // SAFETY: valid process handle.
        unsafe {
            TerminateProcess(self.process, 1);
        }
    }

    pub fn wait(&self) -> Option<i32> {
        // SAFETY: valid process handle.
        unsafe {
            WaitForSingleObject(self.process, 10_000);
        }
        self.try_wait()
    }
}

impl AsRawHandle for LowChild {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.process as _
    }
}

impl Drop for LowChild {
    fn drop(&mut self) {
        // SAFETY: closed exactly once.
        unsafe {
            if !self.resumed {
                TerminateProcess(self.process, 1);
            }
            CloseHandle(self.thread);
            CloseHandle(self.process);
        }
    }
}

/// Label a folder (and what is created in it) Low, so a Low process may write there. Uses the
/// system `icacls`, which needs no elevation for a folder you own.
pub fn label_low(dir: &Path) -> Result<(), String> {
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/setintegritylevel", "(OI)(CI)L"])
        .output()
        .map_err(|e| format!("icacls: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("icacls failed: {}", String::from_utf8_lossy(&out.stdout).trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_so_the_child_reads_them_back_unchanged() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_arg("C:\\dir with space\\"), "\"C:\\dir with space\\\\\"");
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn the_environment_block_is_sorted_and_double_terminated() {
        let b = env_block(&[("b".into(), "2".into()), ("A".into(), "1".into())]);
        let s: String = String::from_utf16_lossy(&b);
        assert_eq!(s, "A=1\0b=2\0\0");
        assert_eq!(env_block(&[]), vec![0, 0]);
    }
}
