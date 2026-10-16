//! System and process tools: `sys.info`, `sys.disks`, `sys.env`, `proc.list`, `proc.info`, `proc.kill`.
//!
//! Reads only report; `proc.kill` names the process it expects (`expect_name`), refuses system
//! and security processes and Aqua itself, and always asks. Process command lines and environment
//! variable values are not returned: they routinely carry secrets.

use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde_json::{json, Value};

/// Processes that are never terminated through these tools.
const PROTECTED_PROCESSES: &[&str] = &[
    "system", "registry", "smss", "csrss", "wininit", "winlogon", "services", "lsass", "lsm", "svchost", "dwm", "fontdrvhost", "explorer", "sihost", "ctfmon", "audiodg",
    "msmpeng", "nissrv", "securityhealthservice", "securityhealthsystray", "mssense", "sense", "aqua-core", "aqua_core",
];

/// Environment variables whose values are safe to show.
const ENV_VALUE_ALLOW: &[&str] = &[
    "OS", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS", "USERNAME", "COMPUTERNAME", "USERDOMAIN", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "PATHEXT", "PATH", "TEMP", "TMP",
    "LANG", "JAVA_HOME", "GOPATH", "CARGO_HOME", "RUSTUP_HOME", "PROCESSOR_IDENTIFIER",
];

fn is_protected(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let n = n.strip_suffix(".exe").unwrap_or(&n);
    PROTECTED_PROCESSES.contains(&n)
}

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub threads: u32,
}

pub struct SysProvider;

impl SysProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SysProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn manifest(name: &str, description: &str, classes: Vec<PermClass>, props: Value, required: &[&str], read_only: bool, untrusted: bool) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: json!({"type": "object", "properties": props, "required": required}),
        timeout_default_ms: 10_000,
        timeout_max_ms: 30_000,
        annotations: Annotations { read_only, destructive: !read_only, idempotent: read_only, open_world: false },
        untrusted_output: untrusted,
    }
}

impl ToolProvider for SysProvider {
    fn id(&self) -> &str {
        "sys"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let read = vec![PermClass::Read];
        vec![
            manifest("sys.info", "Operating system, machine name, CPU count, memory and uptime.", read.clone(), json!({}), &[], true, false),
            manifest("sys.disks", "Drives with total and free space.", read.clone(), json!({}), &[], true, false),
            manifest("sys.env", "List environment variable names; values are shown only for a few harmless ones.", read.clone(),
                json!({"filter": {"type": "string", "maxLength": 60}, "names": {"type": "array", "items": {"type": "string", "maxLength": 100}, "maxItems": 30}}), &[], true, false),
            manifest("proc.list", "List running processes (pid, parent, name, threads). No command lines.", read.clone(),
                json!({"name": {"type": "string", "maxLength": 100}, "sort": {"type": "string", "enum": ["name", "pid", "threads"], "default": "name"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50}}), &[], true, true),
            manifest("proc.info", "Details of one process by pid, optionally with its child processes.", read,
                json!({"pid": {"type": "integer", "minimum": 1}, "include_children": {"type": "boolean", "default": false}}), &["pid"], true, true),
            manifest("proc.kill", "Terminate a process by pid. expect_name must match the process, so a reused pid is never killed by mistake.", vec![PermClass::Write, PermClass::Destructive],
                json!({"pid": {"type": "integer", "minimum": 5}, "expect_name": {"type": "string", "minLength": 1, "maxLength": 100}, "tree": {"type": "boolean", "default": false}}), &["pid", "expect_name"], false, false),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        if tool != "proc.kill" {
            return Ok(Classification { classes: vec![PermClass::Read], target: "this machine".into(), summary: format!("{tool}"), target_allowed: true });
        }
        let pid = args["pid"].as_u64().unwrap_or(0) as u32;
        let expect = args["expect_name"].as_str().unwrap_or("");
        let (target, children) = kill_plan(pid, expect, args["tree"].as_bool().unwrap_or(false))?;
        Ok(Classification {
            classes: vec![PermClass::Write, PermClass::Destructive],
            target: format!("{} (pid {pid})", target.name),
            summary: format!("Terminate {} (pid {pid}){}", target.name, if children.is_empty() { String::new() } else { format!(" and {} child process(es)", children.len()) }),
            target_allowed: false,
        })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || match tool.as_str() {
                "sys.info" => info(),
                "sys.disks" => disks(),
                "sys.env" => env(&args),
                "proc.list" => list(&args),
                "proc.info" => proc_info(&args),
                "proc.kill" => kill(&args),
                other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("system task failed: {e}")))?
        })
    }
}

fn out(data: Value, text: String) -> Result<ProviderOutput, ToolError> {
    Ok(ProviderOutput { data, text, ..Default::default() })
}

fn info() -> Result<ProviderOutput, ToolError> {
    let (total, avail) = memory_mb();
    let up = uptime_s();
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
    let data = json!({"os": std::env::consts::OS, "arch": std::env::consts::ARCH, "hostname": host, "user": user, "cpu_count": cpus, "memory_total_mb": total, "memory_available_mb": avail, "uptime_s": up, "aqua_pid": std::process::id()});
    let text = format!("{} {} on {host} as {user}; {cpus} CPUs; memory {avail} MB free of {total} MB; up {}h {}m", std::env::consts::OS, std::env::consts::ARCH, up / 3600, (up % 3600) / 60);
    out(data, text)
}

fn env(args: &Value) -> Result<ProviderOutput, ToolError> {
    let filter = args["filter"].as_str().unwrap_or("").to_ascii_lowercase();
    let wanted: Vec<String> = args["names"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_ascii_uppercase())).collect()).unwrap_or_default();
    let mut rows: Vec<Value> = std::env::vars()
        .filter(|(k, _)| (filter.is_empty() || k.to_ascii_lowercase().contains(&filter)) && (wanted.is_empty() || wanted.contains(&k.to_ascii_uppercase())))
        .map(|(k, v)| {
            let shown = ENV_VALUE_ALLOW.iter().any(|a| a.eq_ignore_ascii_case(&k));
            json!({"name": k, "value": if shown { Value::String(v) } else { Value::Null }, "hidden": !shown})
        })
        .collect();
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let text = rows.iter().map(|r| format!("{}={}", r["name"].as_str().unwrap_or(""), r["value"].as_str().unwrap_or("(value hidden)"))).collect::<Vec<_>>().join("\n");
    out(json!({ "variables": rows }), if text.is_empty() { "No matching variables.".into() } else { text })
}

fn list(args: &Value) -> Result<ProviderOutput, ToolError> {
    let pattern = args["name"].as_str().unwrap_or("");
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;
    let mut procs = processes()?;
    if !pattern.is_empty() {
        procs.retain(|p| crate::fs::guard::wildcard(&format!("*{pattern}*"), &p.name));
    }
    match args["sort"].as_str().unwrap_or("name") {
        "pid" => procs.sort_by_key(|p| p.pid),
        "threads" => procs.sort_by_key(|p| std::cmp::Reverse(p.threads)),
        _ => procs.sort_by_key(|p| p.name.to_ascii_lowercase()),
    }
    let total = procs.len();
    procs.truncate(limit);
    let rows: Vec<Value> = procs.iter().map(|p| json!({"pid": p.pid, "ppid": p.ppid, "name": p.name, "threads": p.threads})).collect();
    let text = procs.iter().map(|p| format!("{:>6} {:>6} {} ({} threads)", p.pid, p.ppid, p.name, p.threads)).collect::<Vec<_>>().join("\n");
    out(json!({"processes": rows, "total": total, "truncated": total > limit}), if text.is_empty() { "No matching processes.".into() } else { text })
}

fn proc_info(args: &Value) -> Result<ProviderOutput, ToolError> {
    let pid = args["pid"].as_u64().unwrap_or(0) as u32;
    let all = processes()?;
    let p = all.iter().find(|p| p.pid == pid).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no process with pid {pid}")))?;
    let children: Vec<&ProcInfo> = if args["include_children"].as_bool().unwrap_or(false) { descendants(&all, pid) } else { Vec::new() };
    let data = json!({"pid": p.pid, "ppid": p.ppid, "name": p.name, "threads": p.threads, "protected": is_protected(&p.name), "children": children.iter().map(|c| json!({"pid": c.pid, "name": c.name})).collect::<Vec<_>>()});
    let mut text = format!("{} (pid {}, parent {}, {} threads){}", p.name, p.pid, p.ppid, p.threads, if is_protected(&p.name) { " - protected" } else { "" });
    for c in &children {
        text.push_str(&format!("\n  child {} {}", c.pid, c.name));
    }
    out(data, text)
}

fn descendants(all: &[ProcInfo], pid: u32) -> Vec<&ProcInfo> {
    let mut found: Vec<&ProcInfo> = Vec::new();
    let mut frontier = vec![pid];
    while let Some(parent) = frontier.pop() {
        let kids: Vec<&ProcInfo> = all.iter().filter(|p| p.ppid == parent && p.pid != parent && !found.iter().any(|f| f.pid == p.pid)).collect();
        for p in kids {
            found.push(p);
            frontier.push(p.pid);
        }
        if found.len() > 200 {
            break;
        }
    }
    found
}

/// Check a kill request and return the target and the children that would go with it.
fn kill_plan(pid: u32, expect_name: &str, tree: bool) -> Result<(ProcInfo, Vec<ProcInfo>), ToolError> {
    if pid <= 4 || pid == std::process::id() {
        return Err(ToolError::new(ErrorCode::PermissionDenied, "system processes and Aqua itself cannot be terminated"));
    }
    let all = processes()?;
    let target = all.iter().find(|p| p.pid == pid).cloned().ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no process with pid {pid}")))?;
    let norm = |s: &str| s.to_ascii_lowercase().trim_end_matches(".exe").to_string();
    if norm(&target.name) != norm(expect_name) {
        return Err(ToolError::new(ErrorCode::Conflict, format!("pid {pid} is '{}', not '{expect_name}': the pid may have been reused; list processes again", target.name)));
    }
    if is_protected(&target.name) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{}' is a system or security process and is never terminated by these tools", target.name)));
    }
    let children: Vec<ProcInfo> = if tree { descendants(&all, pid).into_iter().cloned().collect() } else { Vec::new() };
    if children.len() > 50 {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{} child processes: too many to terminate at once", children.len())));
    }
    if let Some(p) = children.iter().find(|c| is_protected(&c.name) || c.pid == std::process::id()) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, format!("a child process ('{}') is protected, so the tree is not terminated", p.name)));
    }
    Ok((target, children))
}

fn kill(args: &Value) -> Result<ProviderOutput, ToolError> {
    let pid = args["pid"].as_u64().unwrap_or(0) as u32;
    let (target, children) = kill_plan(pid, args["expect_name"].as_str().unwrap_or(""), args["tree"].as_bool().unwrap_or(false))?;
    // Children first, so a parent cannot respawn them.
    let mut order: Vec<&ProcInfo> = children.iter().rev().collect();
    order.push(&target);
    let mut killed = Vec::new();
    for p in order {
        terminate(p.pid).map_err(|e| ToolError::new(ErrorCode::OsPermissionDenied, format!("could not terminate {} (pid {}): {e}", p.name, p.pid)))?;
        killed.push(json!({"pid": p.pid, "name": p.name}));
    }
    out(json!({ "terminated": killed }), format!("Terminated {} (pid {pid}){}.", target.name, if children.is_empty() { String::new() } else { format!(" and {} child process(es)", children.len()) }))
}

#[cfg(windows)]
mod os {
    use super::ProcInfo;
    use aqua_tool_host::{ErrorCode, ToolError};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::SystemInformation::{GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    pub fn processes() -> Result<Vec<ProcInfo>, ToolError> {
        // SAFETY: the snapshot handle is closed before returning; the entry struct is sized correctly.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return Err(ToolError::new(ErrorCode::BackendError, format!("cannot list processes: {}", std::io::Error::last_os_error())));
            }
            let mut out = Vec::new();
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e);
            while ok != 0 {
                let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                out.push(ProcInfo { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name: String::from_utf16_lossy(&e.szExeFile[..len]), threads: e.cntThreads });
                ok = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
            Ok(out)
        }
    }

    pub fn terminate(pid: u32) -> Result<(), String> {
        // SAFETY: the handle is closed after use.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let ok = TerminateProcess(h, 1);
            let err = std::io::Error::last_os_error();
            CloseHandle(h);
            if ok == 0 {
                return Err(err.to_string());
            }
            Ok(())
        }
    }

    pub fn memory_mb() -> (u64, u64) {
        // SAFETY: the struct is zeroed and its length field is set as the API requires.
        unsafe {
            let mut m: MEMORYSTATUSEX = std::mem::zeroed();
            m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut m) == 0 {
                return (0, 0);
            }
            (m.ullTotalPhys / (1024 * 1024), m.ullAvailPhys / (1024 * 1024))
        }
    }

    pub fn uptime_s() -> u64 {
        // SAFETY: no arguments.
        unsafe { GetTickCount64() / 1000 }
    }

    /// Free bytes on the drive that holds `path` (a folder), as far as the account running Aqua may use them. `None` when the system does not answer.
    pub fn free_bytes_at(path: &std::path::Path) -> Option<u64> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
        // SAFETY: the path is NUL-terminated and outlives the call; the three out pointers are valid for the call.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
        (ok != 0).then_some(free)
    }

    /// (root, kind, total bytes, free bytes)
    pub fn drives() -> Vec<(String, &'static str, u64, u64)> {
        let mut out = Vec::new();
        // SAFETY: buffers are NUL-terminated and outlive the calls.
        unsafe {
            let mask = GetLogicalDrives();
            for i in 0..26u32 {
                if mask & (1 << i) == 0 {
                    continue;
                }
                let root = format!("{}:\\", (b'A' + i as u8) as char);
                let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
                let kind = match GetDriveTypeW(wide.as_ptr()) {
                    2 => "removable",
                    3 => "fixed",
                    4 => "network",
                    5 => "cdrom",
                    6 => "ramdisk",
                    _ => "unknown",
                };
                let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
                if GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) != 0 {
                    out.push((root, kind, total, total_free));
                }
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod os {
    use super::ProcInfo;
    use aqua_tool_host::{ErrorCode, ToolError};

    pub fn processes() -> Result<Vec<ProcInfo>, ToolError> {
        Err(ToolError::new(ErrorCode::UnsupportedPlatform, "process tools are implemented for Windows only so far"))
    }
    pub fn terminate(_pid: u32) -> Result<(), String> {
        Err("not supported on this platform".into())
    }
    pub fn memory_mb() -> (u64, u64) {
        (0, 0)
    }
    pub fn uptime_s() -> u64 {
        0
    }
    pub fn drives() -> Vec<(String, &'static str, u64, u64)> {
        Vec::new()
    }
    pub fn free_bytes_at(_path: &std::path::Path) -> Option<u64> {
        None
    }
}

use os::{drives, memory_mb, processes, terminate, uptime_s};

/// Free bytes on the drive that holds the folder `path` (what the account may use), or `None` when the system does not say. The tool host asks before it keeps the full text of a long result.
pub fn free_bytes_at(path: &std::path::Path) -> Option<u64> {
    os::free_bytes_at(path)
}

/// Memory (total, available, in MB) and drives (root, kind, total bytes, free bytes), for the monitor tools.
pub fn snapshot() -> (u64, u64, Vec<(String, &'static str, u64, u64)>) {
    let (total, avail) = memory_mb();
    (total, avail, drives())
}

fn disks() -> Result<ProviderOutput, ToolError> {
    let rows: Vec<Value> = drives().into_iter().map(|(root, kind, total, free)| json!({"drive": root, "kind": kind, "total_gb": total / (1 << 30), "free_gb": free / (1 << 30), "free_percent": if total > 0 { free * 100 / total } else { 0 }})).collect();
    let text = rows.iter().map(|r| format!("{} {} {} GB free of {} GB ({}%)", r["drive"].as_str().unwrap_or(""), r["kind"].as_str().unwrap_or(""), r["free_gb"], r["total_gb"], r["free_percent"])).collect::<Vec<_>>().join("\n");
    out(json!({ "drives": rows }), if text.is_empty() { "No drives reported.".into() } else { text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_names_match_however_they_are_written() {
        for n in ["lsass.exe", "LSASS.EXE", "System", "svchost.exe", "MsMpEng.exe", "explorer.exe", "aqua-core.exe"] {
            assert!(is_protected(n), "{n}");
        }
        for n in ["notepad.exe", "chrome.exe", "node.exe"] {
            assert!(!is_protected(n), "{n}");
        }
    }

    #[test]
    fn killing_system_or_own_processes_is_refused_before_any_lookup_matters() {
        for pid in [0u32, 4, std::process::id()] {
            assert_eq!(kill_plan(pid, "x", false).unwrap_err().code, ErrorCode::PermissionDenied, "{pid}");
        }
    }

    #[test]
    fn descendants_walk_the_process_tree_without_looping() {
        let mk = |pid, ppid| ProcInfo { pid, ppid, name: format!("p{pid}"), threads: 1 };
        let all = vec![mk(10, 1), mk(11, 10), mk(12, 11), mk(13, 10), mk(20, 2), mk(30, 30)];
        let mut ids: Vec<u32> = descendants(&all, 10).iter().map(|p| p.pid).collect();
        ids.sort();
        assert_eq!(ids, vec![11, 12, 13]);
        assert!(descendants(&all, 30).is_empty(), "a process that is its own parent does not loop");
    }

    #[test]
    fn env_values_are_hidden_unless_allowlisted() {
        std::env::set_var("AQUA_TEST_SECRET_THING", "supersecret-value");
        let r = env(&json!({"filter": "aqua_test_secret"})).unwrap();
        assert!(!r.text.contains("supersecret-value") && r.text.contains("AQUA_TEST_SECRET_THING=(value hidden)"), "{}", r.text);
        assert_eq!(r.data["variables"][0]["hidden"], true);
        std::env::remove_var("AQUA_TEST_SECRET_THING");
    }

    #[cfg(windows)]
    #[test]
    fn live_system_calls_return_plausible_values() {
        let i = info().unwrap();
        assert!(i.data["memory_total_mb"].as_u64().unwrap() > 512 && i.data["cpu_count"].as_u64().unwrap() >= 1);
        let d = disks().unwrap();
        assert!(!d.data["drives"].as_array().unwrap().is_empty());
        let me = std::process::id();
        let found = proc_info(&json!({"pid": me})).unwrap();
        assert_eq!(found.data["pid"], me);
        let l = list(&json!({"limit": 5})).unwrap();
        assert!(l.data["processes"].as_array().unwrap().len() <= 5 && l.data["total"].as_u64().unwrap() > 5);
        // A name mismatch is a conflict, never a kill.
        assert_eq!(kill_plan(me + 1_000_000, "x", false).unwrap_err().code, ErrorCode::NotFound);
    }
}
