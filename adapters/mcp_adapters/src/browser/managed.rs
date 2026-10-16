//! Launch configuration for the managed browser: pinned Playwright MCP driving an installed
//! Edge/Chrome in an Aqua-only profile.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::policy::{BrowserChannel, BrowserPolicyConfig, ProfileMode};
use crate::stdio_mcp::StdioMcpConfig;

/// Directories under `data/browser/` (all git-ignored).
#[derive(Debug, Clone)]
pub struct BrowserDirs {
    pub root: PathBuf,
    pub profile: PathBuf,
    pub artifacts: PathBuf,
    pub uploads: PathBuf,
    pub policy_file: PathBuf,
    pub audit_file: PathBuf,
}

impl BrowserDirs {
    pub fn new(workspace_root: &Path) -> Self {
        Self::under(workspace_root.join("data").join("browser"))
    }

    /// All browser data under `root` (tests use a temporary folder).
    pub fn under(root: PathBuf) -> Self {
        Self {
            profile: root.join("profile"),
            artifacts: root.join("artifacts"),
            uploads: root.join("uploads"),
            policy_file: root.join("policy.json"),
            audit_file: root.join("audit.jsonl"),
            root,
        }
    }

    /// The Aqua-only profile of a browser. Edge keeps the folder it always had (`profile`); Chrome has its own, so that the two browsers never share a profile
    /// folder (what one writes is not for the other).
    pub fn profile_for(&self, channel: BrowserChannel) -> PathBuf {
        match channel {
            BrowserChannel::Msedge => self.profile.clone(),
            BrowserChannel::Chrome => self.root.join("profile-chrome"),
        }
    }

    pub fn create_all(&self) -> std::io::Result<()> {
        for d in [&self.root, &self.profile, &self.artifacts, &self.uploads] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

/// Is this browser installed in one of the places Playwright looks for it (Program Files, Program Files (x86), the user's local programs)?
pub fn browser_installed(channel: BrowserChannel) -> bool {
    let rel = match channel {
        BrowserChannel::Chrome => r"Google\Chrome\Application\chrome.exe",
        BrowserChannel::Msedge => r"Microsoft\Edge\Application\msedge.exe",
    };
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(|v| std::env::var_os(v)).any(|base| Path::new(&base).join(rel).is_file())
}

/// `tools/browser-mcp/node_modules/@playwright/mcp/cli.js` (installed by `npm ci`).
pub fn playwright_mcp_cli(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join("tools")
        .join("browser-mcp")
        .join("node_modules")
        .join("@playwright")
        .join("mcp")
        .join("cli.js")
}

pub fn managed_browser_installed(workspace_root: &Path) -> bool {
    playwright_mcp_cli(workspace_root).is_file()
}

/// Path as Node/Chromium accept it: Windows extended-length paths (`\\?\E:\…`, produced by
/// `canonicalize`) make Node fail to load the script, so the prefix is stripped.
pub fn node_path(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(String::from).unwrap_or(s)
}

/// Origins the browser may never request, even as sub-resources (Aqua's own services).
/// Playwright documents this as defence in depth, not a boundary; Aqua's policy is the boundary.
const BLOCKED_ORIGINS: &[&str] = &[
    "http://127.0.0.1:8766",
    "http://127.0.0.1:8767",
    "http://localhost:8766",
    "http://localhost:8767",
    // Cloud metadata services: the first place a redirect or an injected page would send a request for credentials.
    "http://169.254.169.254",
    "http://169.254.170.2",
    "http://100.100.100.200",
    "http://metadata.google.internal",
    "https://metadata.google.internal",
];

/// What the browser engine itself refuses to request: the fixed list above plus the policy's deny list, written the way
/// the engine reads them. This is checked by the browser before a request leaves it, so it covers the page itself and its
/// sub-requests, which Aqua's own check (after a page has loaded) cannot stop. It does NOT cover a server-side redirect
/// (tested against Edge: the redirect target is still requested) or a name that resolves to a private address; those
/// stay with Aqua's own check, which looks the name up before the page is opened and again after (`documentation/BROWSER.md`). Exact hosts become http and https origins;
/// `*.domain` / `host-*` wildcards and IPv4 / IPv6 ranges are turned into host patterns by [`engine_patterns`]. A range
/// that does not fit (wider than [`MAX_PATTERNS_PER_RULE`], or beyond the first IPv6 group) is left to Aqua's check too.
pub fn blocked_origins(policy: &BrowserPolicyConfig) -> Vec<String> {
    let mut out: Vec<String> = BLOCKED_ORIGINS.iter().map(|s| s.to_string()).collect();
    for entry in &policy.deny {
        for origin in engine_patterns(entry) {
            if !out.contains(&origin) {
                out.push(origin);
            }
        }
    }
    out
}

/// More than this many patterns for one rule is not worth the command line; the rule then stays with Aqua's own check.
const MAX_PATTERNS_PER_RULE: usize = 64;

/// One deny-list entry as patterns the engine understands (`*` matches any characters except `/`).
fn engine_patterns(entry: &str) -> Vec<String> {
    let mut e = entry.trim().to_ascii_lowercase();
    for scheme in ["http://", "https://"] {
        if let Some(rest) = e.strip_prefix(scheme) {
            e = rest.to_string();
        }
    }
    let e = e.trim_end_matches('/').to_string();
    if e.is_empty() || !e.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '*' | '/')) {
        return Vec::new();
    }
    if let Some((net, bits)) = e.split_once('/') {
        return match (net.trim_matches(['[', ']']).parse::<std::net::IpAddr>(), bits.parse::<u8>()) {
            (Ok(std::net::IpAddr::V4(n)), Ok(bits)) if bits <= 32 => ipv4_range_patterns(n.octets(), bits),
            (Ok(std::net::IpAddr::V6(n)), Ok(bits)) if bits <= 128 => ipv6_range_patterns(n.segments()[0], bits),
            _ => Vec::new(),
        };
    }
    if e.contains('*') {
        // `*.domain` also means the bare domain itself (Aqua's rule does); with a port rule only that port, otherwise any port.
        let has_port = e.rsplit_once(':').map(|(h, p)| !h.contains(':') && p.parse::<u16>().is_ok()).unwrap_or(false);
        let mut out = Vec::new();
        let hosts: Vec<String> = match e.strip_prefix("*.") {
            Some(rest) => vec![e.clone(), rest.to_string()],
            None => vec![e.clone()],
        };
        for h in hosts {
            out.push(h.clone());
            if !has_port && !h.ends_with('*') {
                out.push(format!("{h}:*"));
            }
        }
        return out;
    }
    // A plain host or host:port (or [v6]:port): an origin per scheme.
    ["http", "https"].iter().map(|scheme| format!("{scheme}://{e}")).collect()
}

/// `10.0.0.0/8` -> `10.*.*.*`; a range that ends inside an octet (`172.16.0.0/12`) becomes one pattern per value of that octet.
fn ipv4_range_patterns(o: [u8; 4], bits: u8) -> Vec<String> {
    if bits == 0 {
        return Vec::new(); // "everything" is a rule about the policy mode, not something to hand the engine
    }
    let whole = (bits / 8) as usize;
    let rem = bits % 8;
    let (first, count) = if rem == 0 {
        (0u16, 1u16)
    } else {
        let mask = 0xffu8 << (8 - rem);
        ((o[whole] & mask) as u16, 1u16 << (8 - rem))
    };
    if count as usize > MAX_PATTERNS_PER_RULE || whole > 4 {
        return Vec::new();
    }
    (0..count)
        .map(|i| {
            (0..4)
                .map(|k| {
                    if k < whole {
                        o[k].to_string()
                    } else if k == whole && rem > 0 {
                        (first + i).to_string()
                    } else {
                        "*".to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect()
}

/// `fc00::/7` -> `[fc*`, `[fd*`; `fe80::/10` -> `[fe8*` ... `[feb*`. Only ranges that fit inside the first group (at most 16 bits)
/// and whose first group is written with four digits are handled (an address written `[fc00::1]` keeps its four digits).
fn ipv6_range_patterns(first_group: u16, bits: u8) -> Vec<String> {
    if bits == 0 || bits > 16 || first_group < 0x1000 {
        return Vec::new();
    }
    let digits = format!("{first_group:04x}");
    let whole = (bits / 4) as usize;
    let rem = bits % 4;
    if rem == 0 {
        return vec![format!("[{}*", &digits[..whole])];
    }
    let next = u8::from_str_radix(&digits[whole..whole + 1], 16).unwrap_or(0);
    let mask = (0xfu8 << (4 - rem)) & 0xf;
    let base = next & mask;
    (0..(1u8 << (4 - rem))).map(|i| format!("[{}{:x}*", &digits[..whole], base + i)).collect()
}

/// Output folder cap before Playwright evicts old snapshots/screenshots (200 MB).
const OUTPUT_MAX_BYTES: u64 = 200 * 1024 * 1024;

pub fn managed_browser_config(
    workspace_root: &Path,
    dirs: &BrowserDirs,
    policy: &BrowserPolicyConfig,
) -> StdioMcpConfig {
    let node = std::env::var("AQUA_NODE_BIN").unwrap_or_else(|_| "node".into());
    let mut args: Vec<String> = vec![
        node_path(&playwright_mcp_cli(workspace_root)),
        "--browser".into(),
        policy.browser.as_arg().into(),
    ];
    match policy.profile {
        ProfileMode::Persistent => {
            args.push("--user-data-dir".into());
            args.push(node_path(&dirs.profile_for(policy.browser)));
        }
        ProfileMode::Ephemeral => args.push("--isolated".into()),
    }
    if policy.headless {
        args.push("--headless".into());
    }
    let idle_ms = u64::from(policy.idle_shutdown_minutes) * 60_000;
    args.extend([
        // Text-only local model: never send image bytes back.
        "--image-responses".into(),
        "omit".into(),
        // Pages must not be able to register their own tools.
        "--no-webmcp".into(),
        "--codegen".into(),
        "none".into(),
        "--file-paths".into(),
        "absolute".into(),
        "--output-dir".into(),
        node_path(&dirs.artifacts),
        "--output-max-size".into(),
        OUTPUT_MAX_BYTES.to_string(),
        "--timeout-action".into(),
        "5000".into(),
        "--timeout-navigation".into(),
        "30000".into(),
        "--idle-timeout".into(),
        idle_ms.to_string(),
        "--viewport-size".into(),
        "1280x800".into(),
        "--blocked-origins".into(),
        blocked_origins(policy).join(";"),
    ]);
    StdioMcpConfig {
        label: "browser".into(),
        command: node,
        args,
        // Playwright confines file access to its workspace root (the cwd): uploads only.
        cwd: Some(PathBuf::from(node_path(&dirs.uploads))),
        // Navigation timeout (30 s) plus margin; the stdio client kills and restarts past this.
        timeout: Duration::from_secs(45),
        ready_hint: "Managed browser ready (Aqua profile).".into(),
        start_hint: "Not started. Run scripts/setup-managed-browser.ps1, then any browser tool starts it."
            .into(),
        direct_spawn: true,
        env_clear: false,
        env_allow: Vec::new(),
        env: Vec::new(),
        must_stay_on_loopback: None,
        // The managed browser's server is pinned by Aqua, and speaks the initialize handshake.
        known_legacy: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_path_strips_extended_length_prefix() {
        assert_eq!(node_path(Path::new(r"\\?\E:\Personal_AI\Aqua")), r"E:\Personal_AI\Aqua");
        assert_eq!(node_path(Path::new(r"E:\ws")), r"E:\ws");
    }

    #[test]
    fn config_reflects_policy() {
        let root = Path::new("E:/ws");
        let dirs = BrowserDirs::new(root);
        let mut policy = BrowserPolicyConfig::default();
        let cfg = managed_browser_config(root, &dirs, &policy);
        let a = cfg.args.join(" ");
        assert!(a.contains("--browser msedge"));
        assert!(a.contains("--user-data-dir"));
        assert!(!a.contains("--isolated"));
        assert!(!a.contains("--headless"));
        assert!(a.contains("--image-responses omit"));
        assert!(a.contains("--no-webmcp"));
        assert!(a.contains("--idle-timeout 600000"));
        assert!(a.contains("http://169.254.169.254") && a.contains("--blocked-origins"), "metadata endpoints are blocked by the engine: {a}");
        assert!(!a.contains("--caps"));
        assert!(cfg.direct_spawn);
        assert_eq!(cfg.cwd.as_deref(), Some(dirs.uploads.as_path()));

        policy.profile = ProfileMode::Ephemeral;
        policy.headless = true;
        policy.browser = super::super::policy::BrowserChannel::Chrome;
        let a = managed_browser_config(root, &dirs, &policy).args.join(" ");
        assert!(a.contains("--isolated") && a.contains("--headless") && a.contains("--browser chrome"));
        assert!(!a.contains("--user-data-dir"));
        // Each browser has a profile folder of its own: Edge keeps the one it had.
        policy.profile = ProfileMode::Persistent;
        let chrome = managed_browser_config(root, &dirs, &policy).args.join(" ");
        // (The Chrome folder starts with the name of the Edge one, so the Edge folder is looked for as a whole argument.)
        assert!(chrome.contains("profile-chrome") && !chrome.contains(&format!("{} ", node_path(&dirs.profile))), "{chrome}");
        policy.browser = BrowserChannel::Msedge;
        let edge = managed_browser_config(root, &dirs, &policy).args.join(" ");
        assert!(edge.contains(&node_path(&dirs.profile)) && !edge.contains("profile-chrome"), "{edge}");
        assert_eq!(dirs.profile_for(BrowserChannel::Msedge), dirs.profile);
    }

    /// How the engine reads a pattern: `http(s)://host` is an origin; anything else is a host pattern (`*` = any run of
    /// characters except `/`), tried against the host with its port, as the engine does.
    fn engine_blocks(patterns: &[String], url_host: &str) -> bool {
        fn star_match(pat: &str, text: &str) -> bool {
            let parts: Vec<&str> = pat.split('*').collect();
            if parts.len() == 1 {
                return pat == text;
            }
            let mut pos = 0;
            for (i, part) in parts.iter().enumerate() {
                if i == 0 {
                    if !text.starts_with(part) {
                        return false;
                    }
                    pos = part.len();
                } else if i == parts.len() - 1 {
                    return text.len() >= pos + part.len() && text.ends_with(part);
                } else if let Some(f) = text[pos..].find(part) {
                    pos += f + part.len();
                } else {
                    return false;
                }
            }
            true
        }
        patterns.iter().any(|p| match p.split_once("://") {
            Some((_, host)) if !p.starts_with('*') => host == url_host,
            _ => star_match(p, url_host),
        })
    }

    #[test]
    fn exact_denied_hosts_become_origins_and_unusable_rules_are_dropped() {
        let mut policy = BrowserPolicyConfig::default();
        policy.deny = vec!["evil.example".into(), "10.1.2.3:8080".into(), "bad host".into(), "".into(), "0.0.0.0/0".into()];
        let origins = blocked_origins(&policy);
        for want in ["http://evil.example", "https://evil.example", "http://10.1.2.3:8080", "https://10.1.2.3:8080", "http://127.0.0.1:8767", "http://169.254.169.254"] {
            assert!(origins.iter().any(|o| o == want), "{want} missing from {origins:?}");
        }
        assert!(!origins.iter().any(|o| o.contains("bad host") || o == "*" || o.contains("0.0.0.0")), "{origins:?}");
        assert_eq!(origins.len(), blocked_origins(&policy).len());
        let mut sorted = origins.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), origins.len(), "no duplicates");
    }

    #[test]
    fn wildcard_hosts_are_blocked_before_the_request_leaves_the_browser() {
        let mut policy = BrowserPolicyConfig::default();
        policy.deny = vec!["*.tracker.net".into(), "ads-*.example.com".into(), "*.cdn.test:8443".into()];
        let o = blocked_origins(&policy);
        for yes in ["a.tracker.net", "a.b.tracker.net", "tracker.net", "a.tracker.net:8080", "ads-7.example.com", "ads-x.example.com:81", "x.cdn.test:8443"] {
            assert!(engine_blocks(&o, yes), "{yes} must be blocked: {o:?}");
        }
        for no in ["tracker.net.evil.org", "nottracker.net", "example.com", "x.cdn.test", "x.cdn.test:8444", "wikipedia.org"] {
            assert!(!engine_blocks(&o, no), "{no} must not be blocked: {o:?}");
        }
    }

    #[test]
    fn the_default_private_ranges_are_blocked_as_address_patterns() {
        let o = blocked_origins(&BrowserPolicyConfig::default());
        for yes in [
            "10.0.0.1", "10.255.255.255", "10.1.2.3:8080", "172.16.0.1", "172.31.255.254", "172.20.1.1:3000", "192.168.1.1", "192.168.0.10:80",
            "169.254.169.254", "169.254.1.1", "100.64.0.1", "100.127.255.255", "0.1.2.3", "[fc00::1]", "[fd12:3456::1]:80", "[fe80::1]", "[febf::1]",
        ] {
            assert!(engine_blocks(&o, yes), "{yes} must be blocked");
        }
        for no in [
            "8.8.8.8", "172.15.0.1", "172.32.0.1", "192.169.0.1", "100.63.0.1", "100.128.0.1", "11.0.0.1", "1.10.0.1", "wikipedia.org", "[2606:4700::1]", "[fe00::1]", "[fec0::1]", "[fb00::1]",
            "127.0.0.1:3000", // not on the deny list (the local dev server is allowed); only Aqua's own ports are
        ] {
            assert!(!engine_blocks(&o, no), "{no} must not be blocked");
        }
        assert!(engine_blocks(&o, "127.0.0.1:8767") && engine_blocks(&o, "localhost:8766"));
    }

    #[test]
    fn range_translation_is_exact_and_bounded() {
        assert_eq!(ipv4_range_patterns([10, 0, 0, 0], 8), vec!["10.*.*.*"]);
        assert_eq!(ipv4_range_patterns([192, 168, 0, 0], 16), vec!["192.168.*.*"]);
        assert_eq!(ipv4_range_patterns([192, 168, 7, 0], 24), vec!["192.168.7.*"]);
        assert_eq!(ipv4_range_patterns([10, 1, 2, 3], 32), vec!["10.1.2.3"]);
        let r = ipv4_range_patterns([172, 16, 0, 0], 12);
        assert_eq!(r.len(), 16);
        assert_eq!((r.first().unwrap().as_str(), r.last().unwrap().as_str()), ("172.16.*.*", "172.31.*.*"));
        assert_eq!(ipv4_range_patterns([100, 64, 0, 0], 10).len(), 64);
        // too wide for the command line (a /1 would need 128 patterns) or meaningless: left to Aqua's check
        assert!(ipv4_range_patterns([128, 0, 0, 0], 1).is_empty());
        assert!(ipv4_range_patterns([0, 0, 0, 0], 0).is_empty());
        assert_eq!(ipv6_range_patterns(0xfc00, 7), vec!["[fc*", "[fd*"]);
        assert_eq!(ipv6_range_patterns(0xfe80, 10), vec!["[fe8*", "[fe9*", "[fea*", "[feb*"]);
        assert!(ipv6_range_patterns(0x2001, 32).is_empty(), "beyond the first group: left to Aqua's check");
        assert!(ipv6_range_patterns(0x0100, 8).is_empty(), "first group written without its leading zeros is ambiguous");
    }

    #[test]
    fn the_whole_command_line_stays_small() {
        let policy = BrowserPolicyConfig::default();
        let joined = blocked_origins(&policy).join(";");
        assert!(joined.len() < 4000, "{} bytes", joined.len());
    }
}
