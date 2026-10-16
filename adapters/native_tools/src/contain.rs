//! Process containment for commands the tools start.
//!
//! On Windows every child is put in a Job Object that (a) kills the whole tree when the job is
//! closed or terminated, (b) caps memory per process and the number of live processes. This is
//! resource containment, not a security sandbox: a child still runs with the user's rights and can
//! read the user's files and reach the network. The plan's stronger tiers (low integrity, a
//! sandbox user, WSL2) are separate work.
//!
//! Known gap: the child is assigned to the job just after it starts, so a process it spawns in
//! that first instant is not covered. Creating it suspended closes the gap and is planned.

use std::collections::HashMap;
use std::process::Child;

/// Variables every child may inherit. Everything else is dropped.
pub const ENV_ALLOW: &[&str] = &[
    "PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "TEMP", "TMP", "USERPROFILE", "USERNAME", "USERDOMAIN", "HOMEDRIVE", "HOMEPATH",
    "APPDATA", "LOCALAPPDATA", "PROGRAMDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMW6432", "COMPUTERNAME", "OS", "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE", "HOME", "LANG", "LC_ALL", "CARGO_HOME", "RUSTUP_HOME",
    // What real tools need and `sys.env` already shows as harmless (card OA7-01, TM-09): where a proxy and a certificate store are, where Java, Go and a Python environment are, and where
    // the person pointed cargo's build output. They come from this process only: the model cannot set any of them (see `ENV_MODEL_MAY_SET`).
    "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS", "REQUESTS_CA_BUNDLE", "JAVA_HOME", "GOPATH", "GOROOT", "VIRTUAL_ENV", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS",
    "PSMODULEPATH", "ALLUSERSPROFILE", "LOGONSERVER",
];

/// What every child is told about its output, unless the caller says otherwise where it may (card OA7-01, TM-09). A Python that writes to a pipe uses the ANSI code page and a
/// non-Latin `print` raises `UnicodeEncodeError`; colour codes and a terminal that is not one are noise to whoever reads the output. `CI` is not set: it changes what tools do.
pub const ENV_DEFAULTS: &[(&str, &str)] = &[("PYTHONUTF8", "1"), ("PYTHONIOENCODING", "utf-8"), ("NO_COLOR", "1"), ("TERM", "dumb")];

/// Variables the caller (the model) may set. Nothing else, and never a secret.
pub const ENV_MODEL_MAY_SET: &[&str] = &["LANG", "LC_ALL", "NO_COLOR", "FORCE_COLOR", "RUST_BACKTRACE", "RUST_LOG", "CI", "TZ"];

/// The environment for a child: the allowlist from this process, the defaults of [`ENV_DEFAULTS`], then the model's requested
/// variables (checked against [`ENV_MODEL_MAY_SET`]).
pub fn build_env(parent: impl Iterator<Item = (String, String)>, requested: &HashMap<String, String>) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = parent
        .filter(|(k, _)| ENV_ALLOW.iter().any(|a| a.eq_ignore_ascii_case(k)))
        .filter(|(k, _)| !looks_secret(k))
        .filter(|(k, v)| {
            // A proxy address with a user name and a password in it is a credential, and it is not handed on (the program then connects without the proxy, or fails, which is visible).
            let carries = is_proxy_setting(k) && proxy_has_credentials(v);
            if carries {
                tracing::warn!("the environment variable {k} holds a proxy address with a user name or password in it, so programs started by the tools do not get it");
            }
            !carries
        })
        .collect();
    for (k, v) in ENV_DEFAULTS {
        out.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
        out.push((k.to_string(), v.to_string()));
    }
    for (k, v) in requested {
        if !ENV_MODEL_MAY_SET.iter().any(|a| a.eq_ignore_ascii_case(k)) {
            return Err(format!("environment variable '{k}' may not be set (allowed: {})", ENV_MODEL_MAY_SET.join(", ")));
        }
        if v.len() > 256 || v.contains('\0') {
            return Err(format!("value of '{k}' is too long or contains a NUL character"));
        }
        out.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
        out.push((k.clone(), v.clone()));
    }
    Ok(out)
}

fn looks_secret(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "API_KEY", "APIKEY", "PRIVATE_KEY", "ACCESS_KEY"].iter().any(|f| u.contains(f)) || u.starts_with("AQUA_")
}

/// `HTTP_PROXY` and `HTTPS_PROXY`, in either case: the two whose value is an address that can carry a login (`NO_PROXY` is a list of names).
fn is_proxy_setting(name: &str) -> bool {
    name.eq_ignore_ascii_case("HTTP_PROXY") || name.eq_ignore_ascii_case("HTTPS_PROXY")
}

/// Does this proxy address have a user name (and perhaps a password) in front of the host (`http://user:pass@proxy:8080`, or `user:pass@proxy:8080` with no scheme)? The part
/// that counts is the address up to its first `/`, `?` or `#`; an `@` further on belongs to a path or a query and is not a login.
fn proxy_has_credentials(value: &str) -> bool {
    let v = value.trim();
    let rest = v.split_once("://").map_or(v, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

/// Limits for one contained command.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory_mb: u64,
    pub max_processes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { memory_mb: 1024, max_processes: 32 }
    }
}

/// What the Job Object counted while a command ran (card OA7-01, TM-11): the memory limit makes a request for memory fail and the process-count limit makes a new process fail, and
/// neither leaves a trace in the exit code, so the job is asked after the program has ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobUse {
    /// The most memory one process of the command had committed, in bytes (0 when it could not be read).
    pub peak_process_memory: u64,
    /// How many times a new process was refused because the limit of running processes was reached (the job reports each one; 0 when it could not be read).
    pub stopped_at_limit: u32,
    /// How many times a process was refused memory because it was at its limit (0 when it could not be read, and when the job did not say).
    pub refused_memory: u32,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation, JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_UILIMIT_DESKTOP,
        JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    // The two calls of the I/O completion port that the job reports its limit messages to (`windows-sys` has them in a feature this crate does not turn on).
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateIoCompletionPort(file: HANDLE, existing_port: HANDLE, key: usize, threads: u32) -> HANDLE;
        fn GetQueuedCompletionStatus(port: HANDLE, message: *mut u32, key: *mut usize, overlapped: *mut *mut core::ffi::c_void, millis: u32) -> i32;
    }

    /// `JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT` and `JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT`: a new process was refused at the limit of running processes; a process was refused memory.
    const MSG_ACTIVE_PROCESS_LIMIT: u32 = 3;
    const MSG_PROCESS_MEMORY_LIMIT: u32 = 9;

    /// A Job Object holding one command's process tree. Dropping it kills the tree.
    pub struct Contained {
        job: usize,
        /// The completion port the job reports to, or 0 when there is none (the limits still hold; they are just not counted).
        port: usize,
        refused_processes: AtomicU32,
        refused_memory: AtomicU32,
    }

    impl Contained {
        pub fn attach(child: &Child, limits: Limits) -> Result<Self, String> {
            Self::attach_handle(child.as_raw_handle() as HANDLE, limits)
        }

        /// Put a process (by handle) in a new Job Object with the limits, and shut the job off from
        /// the desktop: no reading or writing the clipboard, no changing display or system
        /// settings, no logging off, and no using windows of processes outside the job.
        pub fn attach_handle(process: HANDLE, limits: Limits) -> Result<Self, String> {
            // SAFETY: plain Win32 calls with valid pointers; the handle is closed in Drop.
            unsafe {
                let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(format!("CreateJobObjectW: {}", std::io::Error::last_os_error()));
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.BasicLimitInformation.ActiveProcessLimit = limits.max_processes.max(1);
                info.ProcessMemoryLimit = (limits.memory_mb.max(16) * 1024 * 1024) as usize;
                if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const core::ffi::c_void, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0 {
                    let e = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(format!("SetInformationJobObject: {e}"));
                }
                let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                    UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                        | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                        | JOB_OBJECT_UILIMIT_EXITWINDOWS
                        | JOB_OBJECT_UILIMIT_GLOBALATOMS
                        | JOB_OBJECT_UILIMIT_HANDLES
                        | JOB_OBJECT_UILIMIT_READCLIPBOARD
                        | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
                };
                if SetInformationJobObject(job, JobObjectBasicUIRestrictions, &ui as *const _ as *const core::ffi::c_void, std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32) == 0 {
                    let e = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(format!("SetInformationJobObject(ui): {e}"));
                }
                // The job says when a limit is hit by a message to a completion port (card OA7-01, TM-11). Without one the limits hold all the same, and nothing is counted.
                let mut port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
                if !port.is_null() {
                    let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT { CompletionKey: 1 as *mut core::ffi::c_void, CompletionPort: port };
                    if SetInformationJobObject(job, JobObjectAssociateCompletionPortInformation, &assoc as *const _ as *const core::ffi::c_void, std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32) == 0 {
                        CloseHandle(port);
                        port = std::ptr::null_mut();
                    }
                }
                if AssignProcessToJobObject(job, process) == 0 {
                    let e = std::io::Error::last_os_error();
                    CloseHandle(job);
                    if !port.is_null() {
                        CloseHandle(port);
                    }
                    return Err(format!("AssignProcessToJobObject: {e}"));
                }
                Ok(Self { job: job as usize, port: port as usize, refused_processes: AtomicU32::new(0), refused_memory: AtomicU32::new(0) })
            }
        }

        /// Read the messages the job has sent to its port and count the two that name a limit. The job also reports every process that starts and ends, and these packets wait in the port
        /// until they are read, so a command that runs for hours must be read from time to time: the waiter does it each time it looks at the program.
        pub fn drain(&self) {
            if self.port == 0 {
                return;
            }
            // SAFETY: the port is valid until Drop; the out values are locals.
            unsafe {
                loop {
                    let (mut message, mut key, mut overlapped) = (0u32, 0usize, std::ptr::null_mut());
                    if GetQueuedCompletionStatus(self.port as HANDLE, &mut message, &mut key, &mut overlapped, 0) == 0 {
                        break;
                    }
                    match message {
                        MSG_ACTIVE_PROCESS_LIMIT => {
                            self.refused_processes.fetch_add(1, Ordering::Relaxed);
                        }
                        MSG_PROCESS_MEMORY_LIMIT => {
                            self.refused_memory.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            }
        }

        /// Kill every process in the job now.
        pub fn kill_tree(&self) {
            // SAFETY: the handle is valid until Drop.
            unsafe {
                TerminateJobObject(self.job as HANDLE, 1);
            }
        }

        /// What the job has counted so far: the most memory any one process of it had committed, and what it said about its two limits. A value that cannot be read is 0.
        pub fn usage(&self) -> JobUse {
            self.drain();
            // SAFETY: plain Win32 queries into zeroed structures of the size that is passed; the handle is valid until Drop.
            unsafe {
                let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                let peak = if QueryInformationJobObject(self.job as HANDLE, JobObjectExtendedLimitInformation, &mut ext as *mut _ as *mut core::ffi::c_void, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32, std::ptr::null_mut()) != 0 {
                    ext.PeakProcessMemoryUsed as u64
                } else {
                    0
                };
                let mut acc: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let terminated = if QueryInformationJobObject(self.job as HANDLE, JobObjectBasicAccountingInformation, &mut acc as *mut _ as *mut core::ffi::c_void, std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32, std::ptr::null_mut()) != 0 {
                    acc.TotalTerminatedProcesses
                } else {
                    0
                };
                JobUse { peak_process_memory: peak, stopped_at_limit: self.refused_processes.load(Ordering::Relaxed).max(terminated), refused_memory: self.refused_memory.load(Ordering::Relaxed) }
            }
        }
    }

    impl Drop for Contained {
        fn drop(&mut self) {
            // SAFETY: closed exactly once; KILL_ON_JOB_CLOSE ends the tree.
            unsafe {
                CloseHandle(self.job as HANDLE);
                if self.port != 0 {
                    CloseHandle(self.port as HANDLE);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    /// On other systems only the direct child is killed; process-group containment is planned.
    pub struct Contained(u32);

    impl Contained {
        pub fn attach(child: &Child, _limits: Limits) -> Result<Self, String> {
            Ok(Self(child.id()))
        }

        pub fn kill_tree(&self) {
            let _ = std::process::Command::new("kill").args(["-9", &self.0.to_string()]).status();
        }

        /// Nothing is counted on other systems.
        pub fn drain(&self) {}

        pub fn usage(&self) -> JobUse {
            JobUse::default()
        }
    }
}

pub use imp::Contained;

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> Vec<(String, String)> {
        [("PATH", "p"), ("AQUA_API_TOKEN", "t"), ("GITHUB_TOKEN", "g"), ("AWS_SECRET_ACCESS_KEY", "a"), ("USERPROFILE", "u"), ("RANDOM_VAR", "r"), ("CARGO_HOME", "c")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_child_keeps_what_windows_needs_to_start_and_nothing_that_could_change_what_a_tool_runs() {
        // Windows spells these names its own way; a program started without SystemRoot, TEMP and TMP fails to start or to make a file (Codex 0.160.1 lost them).
        let parent = [
            ("SystemRoot", r"C:\Windows"), ("TEMP", r"C:\T"), ("TMP", r"C:\T"), ("ComSpec", r"C:\Windows\system32\cmd.exe"), ("windir", r"C:\Windows"), ("Path", "p"),
            ("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", "core.fsmonitor"), ("GIT_CONFIG_VALUE_0", "calc"), ("GIT_CONFIG_PARAMETERS", "'core.fsmonitor=calc'"), ("GIT_SSH_COMMAND", "calc"),
            ("GIT_EXTERNAL_DIFF", "calc"), ("NODE_OPTIONS", "--require x"), ("PYTHONSTARTUP", "x.py"), ("PYTHONPATH", "x"), ("LD_PRELOAD", "x"), ("RUSTC_WRAPPER", "calc"), ("CARGO_BUILD_RUSTC", "calc"),
        ];
        let env = build_env(parent.iter().map(|(k, v)| (k.to_string(), v.to_string())), &HashMap::new()).unwrap();
        let has = |n: &str| env.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
        for needed in ["SYSTEMROOT", "TEMP", "TMP", "COMSPEC", "WINDIR", "PATH"] {
            assert!(has(needed), "{needed} is lost: {env:?}");
        }
        for bad in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_PARAMETERS", "GIT_SSH_COMMAND", "GIT_EXTERNAL_DIFF", "NODE_OPTIONS", "PYTHONSTARTUP", "PYTHONPATH", "LD_PRELOAD", "RUSTC_WRAPPER", "CARGO_BUILD_RUSTC"] {
            assert!(!has(bad), "{bad} reached the child");
        }
        // And the model cannot put any of them back.
        for bad in ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS", "GIT_SSH_COMMAND", "GIT_EXTERNAL_DIFF", "NODE_OPTIONS", "PYTHONSTARTUP", "LD_PRELOAD", "PATH", "Path", "COMSPEC", "SYSTEMROOT", "TEMP", "RUSTC_WRAPPER", "HOME"] {
            let asked: HashMap<String, String> = [(bad.to_string(), "x".to_string())].into_iter().collect();
            assert!(build_env(parent.iter().map(|(k, v)| (k.to_string(), v.to_string())), &asked).is_err(), "{bad} may be set by the model");
        }
    }

    #[test]
    fn only_allowlisted_non_secret_variables_are_inherited() {
        let env = build_env(parent().into_iter(), &HashMap::new()).unwrap();
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"PATH") && names.contains(&"USERPROFILE") && names.contains(&"CARGO_HOME"));
        for bad in ["AQUA_API_TOKEN", "GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "RANDOM_VAR"] {
            assert!(!names.contains(&bad), "{bad} leaked");
        }
    }

    /// OA7-01 (TM-09): what real tools need passes from this process, and nothing that looks like a secret comes with it.
    #[test]
    fn the_variables_that_real_tools_need_are_inherited_and_the_model_cannot_set_them() {
        let wanted = [
            ("HTTP_PROXY", "http://proxy.corp:8080"), ("https_proxy", "http://proxy.corp:8080"), ("NO_PROXY", "localhost,.corp"), ("SSL_CERT_FILE", r"C:\certs\ca.pem"), ("NODE_EXTRA_CA_CERTS", r"C:\certs\corp.pem"),
            ("REQUESTS_CA_BUNDLE", r"C:\certs\ca.pem"), ("JAVA_HOME", r"C:\Java\jdk"), ("GOPATH", r"C:\go"), ("GOROOT", r"C:\Go"), ("VIRTUAL_ENV", r"C:\proj\.venv"), ("CARGO_TARGET_DIR", r"D:\build"),
            ("CARGO_BUILD_JOBS", "4"), ("PSModulePath", r"C:\Modules"), ("ALLUSERSPROFILE", r"C:\ProgramData"), ("LOGONSERVER", r"\\PC"),
        ];
        let secrets = [("GITHUB_TOKEN", "t"), ("AWS_SECRET_ACCESS_KEY", "s"), ("NPM_TOKEN", "n"), ("PIP_INDEX_URL", "https://user:pw@pypi.corp/simple"), ("CARGO_REGISTRIES_X_TOKEN", "c"), ("HTTP_PROXY_PASSWORD", "p")];
        let parent: Vec<(String, String)> = wanted.iter().chain(secrets.iter()).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let env = build_env(parent.into_iter(), &HashMap::new()).unwrap();
        let has = |n: &str| env.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
        for (name, _) in wanted {
            assert!(has(name), "{name} was not inherited: {env:?}");
        }
        for (name, _) in secrets {
            assert!(!has(name), "{name} leaked");
        }
        // Only this process decides them: a call cannot set one.
        for (name, _) in wanted {
            let asked: HashMap<String, String> = [(name.to_string(), "x".to_string())].into_iter().collect();
            assert!(build_env(std::iter::empty(), &asked).is_err(), "{name} may be set by the model");
        }
    }

    /// OA7-01 (TM-09): a proxy address with a user name and a password in it is a credential and is not handed on; one without is.
    #[test]
    fn proxy_with_credentials_is_not_inherited() {
        let run = |name: &str, value: &str| build_env([(name.to_string(), value.to_string())].into_iter(), &HashMap::new()).unwrap().iter().any(|(k, _)| k == name);
        for with_login in ["http://user:pass@proxy.corp:8080", "https://user@proxy.corp", "user:pass@proxy.corp:8080", "http://u:p@[::1]:3128/", "HTTP://User:Pw@Proxy"] {
            assert!(!run("HTTP_PROXY", with_login), "{with_login} was handed on");
            assert!(!run("https_proxy", with_login), "{with_login} was handed on as https_proxy");
        }
        for plain in ["http://proxy.corp:8080", "proxy.corp:8080", "http://proxy.corp:8080/pac?user=a@b", "http://10.0.0.1:3128/#frag@x", ""] {
            assert!(run("HTTP_PROXY", plain), "{plain} was dropped");
            assert!(run("HTTPS_PROXY", plain), "{plain} was dropped as HTTPS_PROXY");
        }
        // NO_PROXY is a list of names, not an address with a login.
        assert!(run("NO_PROXY", "localhost,.corp,a@b"));
    }

    /// OA7-01 (TM-09): every child is told to write UTF-8 and plain text, the model may take the colour back, and it cannot touch the Python settings.
    #[test]
    fn a_child_writes_utf8_and_plain_text_unless_the_model_asks_for_colour() {
        let get = |env: &[(String, String)], n: &str| env.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        let env = build_env(parent().into_iter(), &HashMap::new()).unwrap();
        assert_eq!(get(&env, "PYTHONUTF8").as_deref(), Some("1"));
        assert_eq!(get(&env, "PYTHONIOENCODING").as_deref(), Some("utf-8"));
        assert_eq!(get(&env, "NO_COLOR").as_deref(), Some("1"));
        assert_eq!(get(&env, "TERM").as_deref(), Some("dumb"));
        assert_eq!(get(&env, "CI"), None, "CI changes what tools do and is not set for the model");
        // A value in this process's own environment does not take their place, and none is there twice.
        let own: Vec<(String, String)> = [("NO_COLOR", "0"), ("PATH", "p")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let env = build_env(own.into_iter(), &HashMap::new()).unwrap();
        assert_eq!(env.iter().filter(|(k, _)| k.eq_ignore_ascii_case("NO_COLOR")).count(), 1);
        // The model may ask for colour (and says so for CI), not for another encoding.
        let asked: HashMap<String, String> = [("NO_COLOR".to_string(), "".to_string()), ("FORCE_COLOR".to_string(), "1".to_string())].into_iter().collect();
        let env = build_env(parent().into_iter(), &asked).unwrap();
        assert_eq!(get(&env, "NO_COLOR").as_deref(), Some(""));
        assert_eq!(get(&env, "FORCE_COLOR").as_deref(), Some("1"));
        for bad in ["PYTHONUTF8", "PYTHONIOENCODING", "TERM", "PYTHONUNBUFFERED"] {
            let asked: HashMap<String, String> = [(bad.to_string(), "0".to_string())].into_iter().collect();
            assert!(build_env(parent().into_iter(), &asked).is_err(), "{bad} may be set by the model");
        }
    }

    #[test]
    fn the_model_may_only_set_a_few_harmless_variables() {
        let mut ok = HashMap::new();
        ok.insert("RUST_LOG".to_string(), "debug".to_string());
        let env = build_env(parent().into_iter(), &ok).unwrap();
        assert!(env.iter().any(|(k, v)| k == "RUST_LOG" && v == "debug"));
        for bad in ["PATH", "GITHUB_TOKEN", "LD_PRELOAD", "COMSPEC", "NODE_OPTIONS"] {
            let mut m = HashMap::new();
            m.insert(bad.to_string(), "x".to_string());
            assert!(build_env(parent().into_iter(), &m).is_err(), "{bad}");
        }
        let mut long = HashMap::new();
        long.insert("TZ".to_string(), "x".repeat(300));
        assert!(build_env(parent().into_iter(), &long).is_err());
    }
}
