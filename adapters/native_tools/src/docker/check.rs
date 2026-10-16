//! Validation for everything that goes on a docker command line, and a scan of compose files for
//! settings that would give a container control of the host.

use aqua_tool_host::{ErrorCode, ToolError};

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

/// A container name or id: letters, digits, `_`, `.`, `-`; not starting with a symbol.
pub fn container_ref(s: &str) -> Result<&str, ToolError> {
    let ok = !s.is_empty() && s.len() <= 128 && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok { Ok(s) } else { Err(bad(format!("'{s}' is not a valid container name or id"))) }
}

/// An image reference: `name`, `name:tag`, `registry:5000/org/name:tag`, `name@sha256:<hex>`.
pub fn image_ref(s: &str) -> Result<&str, ToolError> {
    if s.is_empty() || s.len() > 255 || s.starts_with(['-', '/', '.', ':']) || s.contains("..") || s.contains("//") || s.contains(char::is_whitespace) {
        return Err(bad(format!("'{s}' is not a valid image reference")));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '@')) {
        return Err(bad(format!("'{s}' contains characters that are not allowed in an image reference")));
    }
    if s.chars().any(|c| c.is_ascii_uppercase()) && !s.contains('@') {
        return Err(bad("image names are lower case"));
    }
    if let Some((_, digest)) = s.split_once('@') {
        let hex = digest.strip_prefix("sha256:").unwrap_or("");
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad("an image digest must be sha256:<64 hex characters>"));
        }
    }
    Ok(s)
}

/// A compose project name: lower-case letters, digits, `_` and `-`.
pub fn project_name(s: &str) -> Result<&str, ToolError> {
    if !s.is_empty() && s.len() <= 63 && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-')) {
        Ok(s)
    } else {
        Err(bad(format!("'{s}' is not a valid compose project name (lower-case letters, digits, '_' and '-')")))
    }
}

pub fn env_name(s: &str) -> Result<&str, ToolError> {
    if !s.is_empty() && s.len() <= 100 && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(s)
    } else {
        Err(bad(format!("'{s}' is not a valid environment variable name")))
    }
}

/// A path inside a container: absolute, no `..`, and not somewhere that hands out control of the host.
pub fn container_path(s: &str) -> Result<&str, ToolError> {
    if !s.starts_with('/') || s.len() > 512 || s.contains("..") || s.contains(['\0', ',', '"', '\n', '\r', '\\']) || s == "/" {
        return Err(bad(format!("'{s}' is not a valid container path (absolute, no '..', not '/')")));
    }
    let l = s.to_ascii_lowercase();
    for banned in ["/var/run/docker.sock", "/run/docker.sock", "/proc", "/sys", "/dev"] {
        if l == banned || l.starts_with(&format!("{banned}/")) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("mounting onto {banned} is not allowed")));
        }
    }
    Ok(s)
}

/// A pattern like `alpine`, `python:3.*`, `ghcr.io/org/*` matched against an image reference.
pub fn image_matches(pattern: &str, image: &str) -> bool {
    crate::fs::guard::wildcard(pattern, image) || (!pattern.contains(':') && !pattern.contains('@') && image.rsplit_once(':').map(|(n, _)| crate::fs::guard::wildcard(pattern, n)).unwrap_or(false))
}

/// Things in a compose file that give a container control of the host or escape the
/// project's sandbox. Returns a short description of each one found.
pub fn scan_compose(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut add = |line: usize, what: &str| problems.push(format!("line {line}: {what}"));
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim().to_ascii_lowercase();
        if line.is_empty() {
            continue;
        }
        let compact = line.replace(' ', "").replace('\'', "").replace('"', "");
        if compact == "privileged:true" || compact.starts_with("privileged:yes") {
            add(n, "privileged: true gives the container the host's full capabilities");
        }
        for key in ["network_mode:host", "pid:host", "ipc:host", "userns_mode:host", "uts:host", "cgroup:host"] {
            if compact == key {
                add(n, &format!("{key} shares the host's namespace"));
            }
        }
        if compact.starts_with("cap_add:") || compact.starts_with("-sys_admin") || compact.starts_with("-all") && line.starts_with("- all") {
            add(n, "adding capabilities is not allowed");
        }
        if compact.starts_with("devices:") || compact.starts_with("device_cgroup_rules:") {
            add(n, "host devices are not allowed");
        }
        if compact.contains("seccomp:unconfined") || compact.contains("apparmor:unconfined") || compact.contains("label:disable") || compact.contains("systempaths=unconfined") {
            add(n, "disabling the container's security profile is not allowed");
        }
        if compact.contains("docker.sock") || compact.contains("docker_engine") {
            add(n, "the Docker socket must never be mounted into a container");
        }
        if compact.starts_with("build:") || compact.starts_with("dockerfile:") {
            add(n, "building images runs arbitrary commands; build them yourself and reference the image");
        }
        // Bind mounts of absolute host paths (short syntax `- /host:/container`, or `source: /host`).
        let t = compact.trim_start_matches('-');
        let bind_short = line.trim_start().starts_with('-') && (t.starts_with('/') || t.starts_with("~/") || t.starts_with("../") || t.as_bytes().get(1) == Some(&b':') && t.as_bytes()[0].is_ascii_alphabetic() && t.contains(":/") || t.starts_with("${"));
        if bind_short && t.contains(':') {
            add(n, "bind mounts in a compose file are not checked against Aqua's allowed folders; use docker.run with mounts instead");
        }
        if compact.starts_with("source:/") || compact.starts_with("source:~") || compact.starts_with("source:..") || compact.starts_with("source:c:") {
            add(n, "bind mount sources in a compose file are not checked against Aqua's allowed folders");
        }
        if compact.starts_with("user:root") || compact == "user:0" || compact == "user:0:0" {
            add(n, "running as root is not allowed here");
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_and_project_names_cannot_be_options() {
        for ok in ["web", "aqua-job_1", "a.b", "0abc"] {
            assert!(container_ref(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-f", "--all", "a b", "a;b", "a/b", "$(x)", "a\nb", &"x".repeat(129), ".hidden", "_x"] {
            assert!(container_ref(bad).is_err(), "{bad:?}");
        }
        assert!(project_name("my-app_2").is_ok());
        for bad in ["", "My App", "-x", "a/b", "A", &"x".repeat(64)] {
            assert!(project_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn image_references_are_checked() {
        let digest = format!("alpine@sha256:{}", "a".repeat(64));
        for ok in ["alpine", "alpine:3.19", "python:3.12-slim", "ghcr.io/org/app:1.0", "localhost:5000/team/app:v2", digest.as_str()] {
            assert!(image_ref(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-it", "--privileged", "Alpine", "a b", "a;b", "../x", "a//b", "alpine@sha256:short", "alpine@md5:abc", "/abs", ":tag", &"a".repeat(256), "img\n"] {
            assert!(image_ref(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn container_paths_refuse_the_socket_and_system_areas() {
        for ok in ["/data", "/app/config", "/srv/www"] {
            assert!(container_path(ok).is_ok(), "{ok}");
        }
        for bad in ["/", "data", "/a/../b", "/var/run/docker.sock", "/run/docker.sock/x", "/proc", "/proc/1", "/sys/fs", "/dev/sda", "/a,b", "/a\"b", "/a\\b"] {
            assert!(container_path(bad).is_err(), "{bad:?}");
        }
        assert!(env_name("APP_MODE").is_ok() && env_name("_x1").is_ok());
        for bad in ["", "1A", "A-B", "A B", "A=B"] {
            assert!(env_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn image_patterns_match_with_or_without_a_tag() {
        assert!(image_matches("alpine", "alpine:3.19") && image_matches("alpine", "alpine"));
        assert!(image_matches("python:3.*", "python:3.12-slim") && !image_matches("python:3.*", "python:2.7"));
        assert!(image_matches("ghcr.io/org/*", "ghcr.io/org/app:1") && !image_matches("ghcr.io/org/*", "ghcr.io/evil/app:1"));
        assert!(!image_matches("alpine", "alpine-evil:1") && !image_matches("alpine", "myalpine"));
    }

    #[test]
    fn dangerous_compose_settings_are_found() {
        let bad = [
            "services:\n  a:\n    privileged: true",
            "services:\n  a:\n    network_mode: \"host\"",
            "services:\n  a:\n    pid: host",
            "services:\n  a:\n    cap_add:\n      - SYS_ADMIN",
            "services:\n  a:\n    devices:\n      - /dev/sda:/dev/sda",
            "services:\n  a:\n    security_opt:\n      - seccomp:unconfined",
            "services:\n  a:\n    volumes:\n      - /var/run/docker.sock:/var/run/docker.sock",
            "services:\n  a:\n    volumes:\n      - //./pipe/docker_engine:/x",
            "services:\n  a:\n    volumes:\n      - /etc:/host-etc:ro",
            "services:\n  a:\n    volumes:\n      - C:/Users/me:/data",
            "services:\n  a:\n    volumes:\n      - type: bind\n        source: /etc\n        target: /x",
            "services:\n  a:\n    build: .",
            "services:\n  a:\n    user: root",
        ];
        for text in bad {
            assert!(!scan_compose(text).is_empty(), "not caught:\n{text}");
        }
        let good = "services:\n  web:\n    image: nginx:1.25\n    ports:\n      - \"127.0.0.1:8080:80\"\n    environment:\n      - MODE=prod\n    volumes:\n      - data:/var/lib/data\n    read_only: true # privileged: true would be bad, but this is a comment\nvolumes:\n  data:\n";
        assert!(scan_compose(good).is_empty(), "{:?}", scan_compose(good));
    }
}
