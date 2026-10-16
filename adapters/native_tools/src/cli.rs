//! Finding and running an external command-line tool (kubectl, terraform, aws…) the same way
//! everywhere: arguments as a list (never a shell), a scrubbed environment, a Job Object, time
//! and output limits, and cancellation.

use crate::contain::{build_env, Limits};
use crate::term::run_command;
use aqua_tool_host::{ErrorCode, ToolError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

pub struct Cli {
    pub name: &'static str,
    pub exe: Option<PathBuf>,
    scratch: PathBuf,
}

impl Cli {
    /// `configured` wins when given; otherwise `<name>.exe` (then `.cmd`) is looked for in `extra_dirs` and on `PATH`.
    /// A configured path that does not exist means "not installed", never a silent fallback.
    pub fn find(name: &'static str, configured: &Option<String>, extra_dirs: &[&str], scratch: &Path) -> Self {
        let exe = match configured {
            Some(p) => {
                let p = PathBuf::from(crate::fs::guard::expand_env(p));
                p.is_file().then_some(p)
            }
            None => search(name, extra_dirs),
        };
        Self { name, exe, scratch: scratch.to_path_buf() }
    }

    pub fn installed(&self) -> bool {
        self.exe.is_some()
    }

    pub fn not_installed(&self) -> String {
        format!("{} is not installed (no {}.exe found). Install it, or set its path in the tool's config file under data/tools.", self.name, self.name)
    }

    /// Run with the scrubbed environment plus `env`. `cwd` defaults to Aqua's scratch folder.
    pub fn run(&self, args: &[String], env: &[(&str, &str)], cwd: Option<&Path>, timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        let exe = self.exe.clone().ok_or_else(|| ToolError::new(ErrorCode::Unavailable, self.not_installed()))?;
        let mut e = build_env(std::env::vars(), &HashMap::new()).unwrap_or_default();
        for (k, v) in env {
            e.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
            e.push((k.to_string(), v.to_string()));
        }
        let r = run_command(exe, args.to_vec(), cwd.map(Path::to_path_buf).unwrap_or_else(|| self.scratch.clone()), e, None, Limits { memory_mb: 1024, max_processes: 32 }, Duration::from_secs(timeout_s), cap_kb * 1024, cancel)?;
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, format!("the run was cancelled and {} was stopped", self.name)));
        }
        if r.timed_out {
            return Err(ToolError::new(ErrorCode::Timeout, format!("{} did not finish within {} and was stopped", self.name, r.limit_words())));
        }
        Ok(Ran { code: r.exit_code, stdout: r.stdout, stderr: r.stderr, truncated: r.truncated })
    }
}

fn search(name: &str, extra_dirs: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = extra_dirs.iter().map(|d| PathBuf::from(crate::fs::guard::expand_env(d))).collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    for d in dirs {
        for ext in ["exe", "cmd"] {
            let p = d.join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

pub fn first_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_path_that_does_not_exist_means_not_installed() {
        let c = Cli::find("kubectl", &Some("C:/nope/kubectl.exe".into()), &[], Path::new("."));
        assert!(!c.installed());
        let r = c.run(&s(&["version"]), &[], None, 5, 4, &AtomicBool::new(false));
        assert_eq!(r.err().map(|e| e.code), Some(ErrorCode::Unavailable));
    }

    #[cfg(windows)]
    #[test]
    fn a_program_on_the_path_is_found_and_run_without_a_shell() {
        let c = Cli::find("hostname", &None, &["%SystemRoot%/System32"], Path::new("."));
        assert!(c.installed());
        let r = c.run(&[], &[], None, 10, 4, &AtomicBool::new(false)).unwrap();
        assert_eq!(r.code, Some(0));
        assert!(!r.stdout.trim().is_empty());
    }
}
