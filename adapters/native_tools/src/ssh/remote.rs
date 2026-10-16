//! Judging a remote command line before it is sent.
//!
//! The remote side runs the command through its own shell, so nothing here can make an arbitrary
//! command safe. The classification only decides how loudly to ask: a short list of plain
//! read-only commands asks once per host per session; everything else asks every time; a few
//! commands that wipe a machine are refused.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// A plain read-only command from the list below.
    ReadOnly,
    /// Anything else: runs code or changes state on the remote machine.
    Changes,
    /// Wipes or bricks a machine, or is a fork bomb.
    Refused,
}

const READ_ONLY: &[&str] = &["uname", "uptime", "df", "du", "free", "whoami", "id", "hostname", "date", "ls", "pwd", "ps", "cat", "head", "tail", "wc", "systemctl", "journalctl", "lsblk", "ip", "ss", "env_summary"];

fn refused_patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"(?i)\brm\s+(-[a-z]*\s+)*-[a-z]*[rf][a-z]*\s+(--no-preserve-root\s+)?(/|/\*|~|~/|\$HOME)(\s|$|;|&|\|)",
            r"(?i)\brm\s+.*--no-preserve-root",
            r"(?i)\bmkfs(\.\w+)?\b",
            r"(?i)\bdd\b[^\n]*\bof=/dev/(sd|hd|nvme|vd|xvd|mmcblk)",
            r">\s*/dev/(sd|hd|nvme|vd|xvd|mmcblk)\w*",
            r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:",
            r"(?i)\bchmod\s+(-[a-z]+\s+)*(0?777|a\+rwx)\s+(-[a-z]+\s+)*/(\s|$)",
            r"(?i)\bwipefs\b",
            r"(?i)\bshred\b[^\n]*\s/dev/",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("pattern compiles"))
        .collect()
    })
}

/// Characters that give a command line a second meaning in a shell.
fn has_shell_syntax(cmd: &str) -> bool {
    cmd.chars().any(|c| matches!(c, ';' | '|' | '&' | '>' | '<' | '$' | '`' | '(' | ')' | '{' | '}' | '\\' | '"' | '\'' | '*' | '?' | '!' | '~' | '\n' | '\r'))
}

pub fn classify(cmd: &str) -> Risk {
    if refused_patterns().iter().any(|re| re.is_match(cmd)) {
        return Risk::Refused;
    }
    if has_shell_syntax(cmd) {
        return Risk::Changes;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let Some(first) = words.first() else { return Risk::Changes };
    if !READ_ONLY.contains(first) {
        return Risk::Changes;
    }
    let follows = words.iter().any(|w| matches!(*w, "-f" | "-F" | "--follow" | "--follow=name"));
    match *first {
        "systemctl" if words.get(1) != Some(&"status") && words.get(1) != Some(&"is-active") && words.get(1) != Some(&"list-units") => Risk::Changes,
        "journalctl" if !words.contains(&"--no-pager") || follows => Risk::Changes,
        "tail" | "head" | "cat" | "ls" | "wc" | "du" if words.iter().any(|w| w.starts_with('/') && sensitive_remote_path(w)) => Risk::Changes,
        "tail" if follows => Risk::Changes,
        "ip" if words.get(1).map(|w| !matches!(*w, "addr" | "a" | "route" | "r" | "link")).unwrap_or(true) => Risk::Changes,
        "ip" if words.iter().any(|w| matches!(*w, "add" | "del" | "set" | "flush" | "replace")) => Risk::Changes,
        "ps" | "df" | "free" | "uname" | "uptime" | "whoami" | "id" | "hostname" | "date" | "pwd" | "lsblk" | "ss" | "env_summary" | "ls" | "cat" | "head" | "tail" | "wc" | "du" | "ip" | "systemctl" | "journalctl" => Risk::ReadOnly,
        _ => Risk::Changes,
    }
}

/// Remote files that hold credentials: reading them is never routine.
fn sensitive_remote_path(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    ["/etc/shadow", "/etc/gshadow", "/etc/sudoers", "/root/", ".ssh/", "id_rsa", "id_ed25519", ".aws/", ".kube/", ".gnupg", "/proc/", ".env", "secret", "credential", "token", ".pgpass", ".netrc"].iter().any(|s| l.contains(s))
}

/// A remote path given to upload/download: no leading dash, no control characters.
pub fn valid_remote_path(p: &str) -> Result<(), &'static str> {
    if p.is_empty() || p.len() > 1024 {
        return Err("remote path must be 1-1024 characters");
    }
    if p.starts_with('-') {
        return Err("remote path may not start with '-'");
    }
    if p.chars().any(|c| c.is_control()) {
        return Err("remote path may not contain control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_read_only_commands_are_recognised() {
        for c in ["uname -a", "uptime", "df -h", "free -m", "whoami", "hostname", "ls -la /var/log", "systemctl status nginx", "journalctl --no-pager -n 50 -u nginx", "tail -n 100 /var/log/syslog", "ps aux", "ip addr", "cat /etc/os-release", "du -sh /var"] {
            assert_eq!(classify(c), Risk::ReadOnly, "{c}");
        }
    }

    #[test]
    fn anything_with_shell_syntax_or_outside_the_list_changes_things() {
        for c in ["ls; rm x", "cat a | sh", "echo hi > /tmp/x", "ls $(whoami)", "ls `id`", "uname && reboot", "ls\nid", "sudo ls", "rm file", "apt install x", "systemctl restart nginx", "systemctl stop nginx", "journalctl -f", "journalctl -n 5", "tail -f /var/log/syslog", "ip link set eth0 down", "ip route add 1.2.3.4 dev x", "bash -c ls", "python3 x.py", "ls *", "cat ~/x"] {
            assert_eq!(classify(c), Risk::Changes, "{c}");
        }
    }

    #[test]
    fn reading_credential_files_is_never_routine() {
        for c in ["cat /etc/shadow", "cat /root/.bash_history", "tail /home/u/.ssh/id_rsa", "cat /home/u/.aws/credentials", "head /proc/1/environ", "cat /srv/app/.env", "cat /etc/sudoers"] {
            assert_eq!(classify(c), Risk::Changes, "{c}");
        }
    }

    #[test]
    fn machine_wiping_commands_are_refused_however_they_are_written() {
        for c in ["rm -rf /", "rm -rf /*", "rm -fr / ", "rm -r -f /", "rm -rf --no-preserve-root /", "rm -rf ~", "rm -rf $HOME", "mkfs.ext4 /dev/sda1", "mkfs /dev/sdb", "dd if=/dev/zero of=/dev/sda", "dd if=x of=/dev/nvme0n1 bs=1M", "cat x > /dev/sda", ":(){ :|:& };:", "chmod -R 777 /", "wipefs -a /dev/sda", "shred -n 3 /dev/sda", "ls && rm -rf /"] {
            assert_eq!(classify(c), Risk::Refused, "{c}");
        }
        // Ordinary deletes are only "changes": they ask, every time.
        for c in ["rm -rf /tmp/build", "rm old.log", "rm -rf ./cache"] {
            assert_eq!(classify(c), Risk::Changes, "{c}");
        }
    }

    #[test]
    fn remote_paths_are_checked() {
        assert!(valid_remote_path("/var/log/app.log").is_ok() && valid_remote_path("relative/x.txt").is_ok());
        for bad in ["", "-rf", "-oProxyCommand=x", "a\nb", "a\0b", &"x".repeat(2000)] {
            assert!(valid_remote_path(bad).is_err(), "{bad}");
        }
    }
}
