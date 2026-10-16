//! Browser policy: which sites, tools and actions Aqua's agent may use in a browser.
//!
//! Loaded from `data/browser/policy.json` (created from defaults on first run). Every browser
//! tool call passes through these checks in [`super::backend::BrowserBackend::call`].

use anyhow::{Context, Result};
use aqua_tool_host::net_addr::{is_loopback_or_unspecified, is_non_public};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteMode {
    /// Unknown sites pause the agent until the user allows or denies them.
    Ask,
    /// Only `allow` rules may be opened.
    Allowlist,
    /// Anything not matching `deny` may be opened.
    Denylist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JavascriptPolicy {
    Off,
    AllowedSites,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMode {
    /// Aqua-only profile on disk (`data/browser/profile`); logins persist.
    Persistent,
    /// In-memory profile, discarded when the browser closes.
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserChannel {
    Msedge,
    Chrome,
}

impl BrowserChannel {
    /// A browser as a person or the model names it: "chrome", "google chrome", "edge", "msedge", "microsoft edge".
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "chrome" | "google chrome" | "google-chrome" => Some(BrowserChannel::Chrome),
            "edge" | "msedge" | "microsoft edge" | "microsoft-edge" => Some(BrowserChannel::Msedge),
            _ => None,
        }
    }

    /// The browser's name as it is said to the person.
    pub fn label(self) -> &'static str {
        match self {
            BrowserChannel::Msedge => "Microsoft Edge",
            BrowserChannel::Chrome => "Google Chrome",
        }
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            BrowserChannel::Msedge => "msedge",
            BrowserChannel::Chrome => "chrome",
        }
    }
}

/// The browser a request names: "in Chrome go to Facebook", "using Edge", "open Google Chrome and ...", "the Chrome browser". A browser is named when its word
/// follows "in", "on", "using", "with", "open", "use" and the like (leaving out "google", "my", "microsoft", "new" between), or is followed by "browser"; "the cutting
/// edge of AI", "search my PC for Chrome" and "install the Chrome extension" name none. When a request names both, or neither, it names none.
pub fn requested_channel(text: &str) -> Option<BrowserChannel> {
    const BEFORE: &[&str] = &["in", "on", "into", "using", "with", "via", "through", "open", "use", "launch", "start", "from"];
    const SKIPPED: &[&str] = &["google", "my", "microsoft", "new"];
    let lower = text.to_ascii_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut found: Vec<BrowserChannel> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let channel = match *w {
            "chrome" => BrowserChannel::Chrome,
            "edge" | "msedge" => BrowserChannel::Msedge,
            _ => continue,
        };
        let mut j = i;
        while j > 0 && SKIPPED.contains(&words[j - 1]) {
            j -= 1;
        }
        let after_a_word_that_names_a_tool = j > 0 && BEFORE.contains(&words[j - 1]);
        let before_the_word_browser = words.get(i + 1) == Some(&"browser");
        // "chrome and edge": the second browser of a pair is named too.
        let joined_to_another = j > 1 && ["and", "or"].contains(&words[j - 1]) && ["chrome", "edge", "msedge"].contains(&words[j - 2]);
        if (after_a_word_that_names_a_tool || before_the_word_browser || joined_to_another || *w == "msedge") && !found.contains(&channel) {
            found.push(channel);
        }
    }
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserPolicyConfig {
    pub mode: SiteMode,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// Host names the person says belong to their own network (`nas.lan`, a dev machine, a split-horizon name): a name that resolves to a private or local address is
    /// refused (see [`BrowserPolicyConfig::check_url_resolving`]) unless a rule here names it. Site rules as in `allow`. It lifts only that refusal: a `deny` rule and Aqua's own
    /// services still win. Empty by default.
    pub allow_private: Vec<String>,
    pub javascript: JavascriptPolicy,
    pub javascript_sites: Vec<String>,
    pub confirm_irreversible: bool,
    pub confirm_timeout_secs: u64,
    pub profile: ProfileMode,
    pub browser: BrowserChannel,
    pub headless: bool,
    pub max_tabs: u32,
    pub idle_shutdown_minutes: u32,
    /// Characters of page snapshot handed to the model per observation.
    pub snapshot_budget_chars: usize,
}

impl Default for BrowserPolicyConfig {
    fn default() -> Self {
        Self {
            mode: SiteMode::Ask,
            allow: [
                "localhost",
                "127.0.0.1",
                "*.bbc.com",
                "*.bbc.co.uk",
                "*.duckduckgo.com",
                "*.wikipedia.org",
                "wttr.in",
            ]
            .map(String::from)
            .to_vec(),
            deny: [
                // Aqua's own local services.
                "127.0.0.1:1420",
                "127.0.0.1:8766",
                "127.0.0.1:8767",
                "localhost:1420",
                "localhost:8766",
                "localhost:8767",
                "[::1]:1420",
                "[::1]:8766",
                "[::1]:8767",
                // Private, link-local and cloud-metadata ranges.
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "169.254.0.0/16",
                "100.64.0.0/10",
                "0.0.0.0/8",
                "fc00::/7",
                "fe80::/10",
                "metadata.google.internal",
            ]
            .map(String::from)
            .to_vec(),
            allow_private: Vec::new(),
            javascript: JavascriptPolicy::Off,
            javascript_sites: Vec::new(),
            confirm_irreversible: true,
            confirm_timeout_secs: 60,
            profile: ProfileMode::Persistent,
            browser: BrowserChannel::Msedge,
            headless: false,
            max_tabs: 4,
            idle_shutdown_minutes: 10,
            snapshot_budget_chars: 6000,
        }
    }
}

impl BrowserPolicyConfig {
    /// Load `path`, or create it from defaults when missing. A malformed file is an error rather
    /// than a silent fallback, so a typo can't quietly widen access.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let cfg: Self = serde_json::from_str(&raw)
                    .with_context(|| format!("invalid browser policy {}", path.display()))?;
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        aqua_tool_host::fsutil::write_atomic_durable(path, &serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!((1..=20).contains(&self.max_tabs), "max_tabs must be 1-20");
        anyhow::ensure!(
            (1000..=100_000).contains(&self.snapshot_budget_chars),
            "snapshot_budget_chars must be 1000-100000"
        );
        anyhow::ensure!(
            (5..=600).contains(&self.confirm_timeout_secs),
            "confirm_timeout_secs must be 5-600"
        );
        for rule in self.allow.iter().chain(&self.deny).chain(&self.allow_private).chain(&self.javascript_sites) {
            anyhow::ensure!(!rule.trim().is_empty(), "empty site rule");
        }
        Ok(())
    }

    /// Decide whether the browser may open `raw_url`.
    pub fn check_url(&self, raw_url: &str) -> UrlDecision {
        self.check_url_with(raw_url, crate::full_access::is_on())
    }

    /// `check_url` with the full-access switch given. Under it a site that is not on any list opens without asking; the deny
    /// rules, private and local addresses, Aqua's own services and the allowlist mode are decided exactly as before.
    pub fn check_url_with(&self, raw_url: &str, full_access: bool) -> UrlDecision {
        let raw = raw_url.trim();
        if raw.eq_ignore_ascii_case("about:blank") {
            return UrlDecision::Allow;
        }
        let Ok(url) = url::Url::parse(raw) else {
            return UrlDecision::Deny(format!("not a valid absolute URL: {raw}"));
        };
        if !matches!(url.scheme(), "http" | "https") {
            return UrlDecision::Deny(format!(
                "scheme \"{}:\" is not allowed (only http/https)",
                url.scheme()
            ));
        }
        let Some(target) = SiteTarget::from_url(&url) else {
            return UrlDecision::Deny(format!("URL has no host: {raw}"));
        };
        // Aqua's own services are never reachable, whatever the policy file says: the UI on 1420
        // sends the approving token to the gateway, so a page the agent drives could otherwise
        // load it and answer its own approval prompts.
        if is_aqua_service(&target) {
            return UrlDecision::Deny(format!("{} is one of Aqua's own local services", target.display()));
        }
        if let Some(rule) = self.deny.iter().find(|r| rule_matches(r, &target)) {
            return UrlDecision::Deny(format!("{} matches deny rule \"{rule}\"", target.display()));
        }
        let allowed = self.allow.iter().any(|r| rule_matches(r, &target));
        if let Some(ip) = target.ip {
            if is_non_public(ip) && !allowed {
                return UrlDecision::Deny(format!(
                    "{} is a private/local network address",
                    target.display()
                ));
            }
        }
        if allowed {
            return UrlDecision::Allow;
        }
        match self.mode {
            SiteMode::Denylist => UrlDecision::Allow,
            SiteMode::Allowlist => {
                UrlDecision::Deny(format!("{} is not in the allowlist", target.display()))
            }
            SiteMode::Ask if full_access => UrlDecision::Allow,
            SiteMode::Ask => UrlDecision::Ask {
                site: target.host.clone(),
                origin: url.origin().ascii_serialization(),
            },
        }
    }

    /// [`check_url_with`](Self::check_url_with), and then the **addresses** of a host name (audit item SE-09). The rules above compare names; a name can point anywhere
    /// (`anything.localtest.me` and `127.0.0.1.nip.io` are public-looking names for this computer, a rebinding name flips to a private address after the first answer), so a
    /// page on Docker, Ollama, a router or Aqua's own gateway could be opened under a harmless name, and under full access without a question. This looks the name up and:
    /// - refuses it when it resolves to Aqua's own services (loopback or unspecified on 1420, 8766, 8767), whatever `allow_private` says;
    /// - refuses it when a resolved address matches a `deny` rule (**deny always wins**: a name in `allow_private` that resolves into a denied range stays denied until the
    ///   range is taken off `deny`, as for an address typed as a number);
    /// - refuses it when any resolved address is private, loopback, link-local or otherwise not public, unless an `allow_private` rule names the name (one bad answer in a mix
    ///   refuses the whole name; being on the `allow` list does not lift this: `allow` trusts a site's pages, not where its name may point);
    /// - when the name does not resolve at all (offline, no such name) it is an unknown host: a name the person listed, or the person's `denylist` mode, stays as decided;
    ///   anything else that would have opened without a question (full access) asks instead.
    ///
    /// Not looked up: an address written as a number (judged above), `localhost` and `*.localhost` (the browser resolves them to this computer by itself and the rules above judge
    /// them by name), `about:blank`, and whatever was already refused. The browser resolves the name again on its own, so a name that answers differently the second time passes this
    /// check and is caught only by the look-up after the page has loaded (`BrowserBackend`, the post-action check); the page's real peer address is not visible through the MCP
    /// servers (documentation/BROWSER.md).
    pub fn check_url_resolving(&self, raw_url: &str, full_access: bool, resolver: &dyn Resolver) -> UrlDecision {
        let decision = self.check_url_with(raw_url, full_access);
        if matches!(decision, UrlDecision::Deny(_)) {
            return decision;
        }
        let Ok(url) = url::Url::parse(raw_url.trim()) else { return decision };
        let Some(target) = SiteTarget::from_url(&url) else { return decision };
        if target.ip.is_some() || target.host == "localhost" || target.host.ends_with(".localhost") {
            return decision;
        }
        let port = target.port.unwrap_or(80);
        let answers: Vec<IpAddr> = match resolver.lookup(&target.host, port) {
            Ok(found) => found.into_iter().map(unmap_ipv4).collect(),
            Err(_) => Vec::new(),
        };
        if answers.is_empty() {
            let asks = UrlDecision::Ask { site: target.host.clone(), origin: url.origin().ascii_serialization() };
            return match decision {
                UrlDecision::Allow if !self.lists(raw_url) && self.mode == SiteMode::Ask => asks,
                other => other,
            };
        }
        for ip in &answers {
            if is_loopback_or_unspecified(*ip) && target.port.is_some_and(|p| AQUA_SERVICE_PORTS.contains(&p)) {
                return UrlDecision::Deny(format!("{} resolves to {ip}, which is one of Aqua's own local services", target.display()));
            }
            let at = SiteTarget { host: if ip.is_ipv6() { format!("[{ip}]") } else { ip.to_string() }, ip: Some(*ip), port: target.port };
            if let Some(rule) = self.deny.iter().find(|r| rule_matches(r, &at)) {
                return UrlDecision::Deny(format!("{} resolves to {ip}, which matches deny rule \"{rule}\"", target.display()));
            }
        }
        if let Some(ip) = answers.iter().find(|ip| is_non_public(**ip)) {
            if !self.allow_private.iter().any(|r| rule_matches(r, &target)) {
                return UrlDecision::Deny(format!(
                    "{} resolves to {ip}, a private/local network address. If this name is on your own network, add it to allow_private",
                    target.display()
                ));
            }
        }
        decision
    }

    /// Is this a site that only the full-access switch lets open without a question? It is on no list of the person's, and the policy would have asked about it
    /// (`SiteMode::Ask`), but under full access it opens silently. A page that planted `https://collector.example/?d=<what the run knows>` is followed there with no
    /// prompt, which is why a run that has read outside text asks about exactly these (R6-08). A listed site, a site in `denylist` mode (the person chose to open
    /// everything but what is denied), and everything the policy denies are not this.
    pub fn opens_silently_only_by_full_access(&self, raw_url: &str, full_access: bool) -> bool {
        full_access && self.check_url_with(raw_url, true) == UrlDecision::Allow && self.check_url_with(raw_url, false) != UrlDecision::Allow
    }

    /// Is this a site on none of the person's lists that opens without a question, **whatever the reason** (card R6-08): full access, or the person's own choice of `denylist`
    /// mode (everything opens but what is denied)? A run that has read outside text asks about exactly these, in every site mode, because an address nobody listed is where
    /// data leaves (`https://collector.example/?d=...`). [`opens_silently_only_by_full_access`](Self::opens_silently_only_by_full_access) is the part of it that full access
    /// causes; a listed site, a site the policy asks about or denies, and `about:blank` are not this.
    pub fn opens_unlisted_without_a_question(&self, raw_url: &str, full_access: bool) -> bool {
        self.check_url_with(raw_url, full_access) == UrlDecision::Allow && !self.lists(raw_url)
    }

    /// Does an allow rule of the person's name this address? An address that is not a site with a host (`about:blank`) counts as listed: there is nothing to leak to.
    fn lists(&self, raw_url: &str) -> bool {
        let Some(target) = url::Url::parse(raw_url.trim()).ok().and_then(|u| SiteTarget::from_url(&u)) else {
            return true;
        };
        self.allow.iter().any(|r| rule_matches(r, &target))
    }

    /// JavaScript evaluation is permitted on the page at `page_url`.
    pub fn javascript_allowed(&self, page_url: Option<&str>) -> bool {
        match self.javascript {
            JavascriptPolicy::Off => false,
            JavascriptPolicy::AllowedSites => page_url
                .and_then(|u| url::Url::parse(u).ok())
                .and_then(|u| SiteTarget::from_url(&u))
                .is_some_and(|t| self.javascript_sites.iter().any(|r| rule_matches(r, &t))),
        }
    }

    /// Take a site off the allow list (the person revoked it). `false` when it was not on it.
    pub fn remove_allowed_site(&mut self, site: &str) -> bool {
        let before = self.allow.len();
        self.allow.retain(|r| !r.eq_ignore_ascii_case(site.trim()));
        self.allow.len() != before
    }

    /// Remember a site the user chose to "always allow".
    pub fn add_allowed_site(&mut self, site: &str) -> bool {
        let site = site.trim().to_ascii_lowercase();
        if site.is_empty() || self.allow.iter().any(|r| r.eq_ignore_ascii_case(&site)) {
            return false;
        }
        self.allow.push(site);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlDecision {
    Allow,
    Deny(String),
    /// Unknown site in `ask` mode: `site` is the host to remember if the user allows it.
    Ask { site: String, origin: String },
}

#[derive(Debug, Clone)]
struct SiteTarget {
    host: String,
    ip: Option<IpAddr>,
    port: Option<u16>,
}

impl SiteTarget {
    fn from_url(url: &url::Url) -> Option<Self> {
        let host = url.host()?;
        let (host_str, ip) = match host {
            // "localhost." (a trailing dot) is the same host as "localhost": rules and the local-service check must
            // see one name.
            url::Host::Domain(d) => (d.trim_end_matches('.').to_ascii_lowercase(), None),
            url::Host::Ipv4(v4) => (v4.to_string(), Some(IpAddr::V4(v4))),
            // "[::ffff:127.0.0.1]" is the IPv4 address 127.0.0.1 written another way: judge it as that address,
            // or every loopback and private-network rule is skipped by wrapping the address in IPv6.
            url::Host::Ipv6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => (v4.to_string(), Some(IpAddr::V4(v4))),
                None => (format!("[{v6}]"), Some(IpAddr::V6(v6))),
            },
        };
        Some(Self {
            host: host_str,
            ip,
            port: url.port_or_known_default(),
        })
    }

    fn display(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Does this address point at the computer itself or its private network (loopback, a private range, `localhost`,
/// `*.localhost`), however it is written? A spoken request should never be able to make Aqua read another local
/// service (a database console, n8n, a router page) just because `localhost` is on the default allow list.
pub fn url_targets_local_machine(raw_url: &str) -> bool {
    let Ok(url) = url::Url::parse(raw_url.trim()) else {
        return false;
    };
    let Some(target) = SiteTarget::from_url(&url) else {
        return false;
    };
    match target.ip {
        Some(ip) => is_non_public(ip),
        None => target.host == "localhost" || target.host.ends_with(".localhost") || !target.host.contains('.'),
    }
}

/// Ports of Aqua's own services: the admin UI, the voice daemon and the API gateway.
const AQUA_SERVICE_PORTS: &[u16] = &[1420, 8766, 8767];

/// True for the loopback (and unspecified) addresses on one of [`AQUA_SERVICE_PORTS`].
fn is_aqua_service(target: &SiteTarget) -> bool {
    let local = match target.ip {
        Some(ip) => ip.is_loopback() || ip.is_unspecified(),
        None => target.host == "localhost" || target.host.ends_with(".localhost"),
    };
    local && target.port.is_some_and(|p| AQUA_SERVICE_PORTS.contains(&p))
}

/// Match one site rule: `host`, `*.domain`, `host:port`, `[v6]:port`, glob with `*`, or CIDR.
/// A leading `http://`/`https://` and trailing `/` are ignored.
fn rule_matches(rule: &str, target: &SiteTarget) -> bool {
    let mut r = rule.trim().to_ascii_lowercase();
    for scheme in ["http://", "https://"] {
        if let Some(rest) = r.strip_prefix(scheme) {
            r = rest.to_string();
        }
    }
    let r = r.trim_end_matches('/');
    if r.is_empty() {
        return false;
    }

    if let Some((net, bits)) = r.split_once('/') {
        return match (target.ip, net.trim_matches(['[', ']']).parse::<IpAddr>(), bits.parse::<u8>()) {
            (Some(ip), Ok(net), Ok(bits)) => cidr_contains(net, bits, ip),
            _ => false,
        };
    }

    let (host_pat, port) = split_host_port(r);
    if let Some(p) = port {
        if target.port != Some(p) {
            return false;
        }
    }
    if let Some(suffix) = host_pat.strip_prefix("*.") {
        return target.host == suffix || target.host.ends_with(&format!(".{suffix}"));
    }
    glob_match(host_pat, &target.host)
}

fn split_host_port(r: &str) -> (&str, Option<u16>) {
    if r.starts_with('[') {
        if let Some(end) = r.find(']') {
            let host = &r[..=end];
            let port = r[end + 1..].strip_prefix(':').and_then(|p| p.parse().ok());
            return (host, port);
        }
        return (r, None);
    }
    match r.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h, Some(port)),
            Err(_) => (r, None),
        },
        _ => (r, None),
    }
}

/// `*` matches any run of characters (including none); everything else is literal.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return text.len() >= pos + part.len() && text.ends_with(part);
        } else if let Some(found) = text[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    true
}

fn cidr_contains(net: IpAddr, bits: u8, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(a)) if bits <= 32 => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            (u32::from(n) & mask) == (u32::from(a) & mask)
        }
        (IpAddr::V6(n), IpAddr::V6(a)) if bits <= 128 => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            (u128::from(n) & mask) == (u128::from(a) & mask)
        }
        _ => false,
    }
}

/// `::ffff:10.0.0.5` is the IPv4 address `10.0.0.5` written another way: the rules are written for the IPv4 form.
fn unmap_ipv4(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Turns a host name into the addresses a connection to it would go to. The site policy asks it before a page is opened and again after the page has loaded
/// ([`BrowserPolicyConfig::check_url_resolving`]); a test gives it fixed answers.
pub trait Resolver: Send + Sync {
    /// Every address `host` resolves to for `port`. `Err` when it does not resolve (offline, no such name) or the answer took too long.
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
}

/// The computer's own resolver, with a time limit so that a look-up that hangs cannot hang the agent with it.
pub struct SystemResolver;

/// How long a look-up may take before the name counts as not resolving.
const LOOKUP_LIMIT: Duration = Duration::from_secs(4);

impl Resolver for SystemResolver {
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        use std::net::ToSocketAddrs;
        let (tx, rx) = std::sync::mpsc::channel();
        let name = host.to_string();
        std::thread::spawn(move || {
            let _ = tx.send((name.as_str(), port).to_socket_addrs().map(|found| found.map(|a| a.ip()).collect::<Vec<_>>()));
        });
        match rx.recv_timeout(LOOKUP_LIMIT) {
            Ok(answer) => answer,
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{host} did not resolve in {} seconds", LOOKUP_LIMIT.as_secs()))),
        }
    }
}

/// Tools the agent may call on the managed (Playwright MCP) backend. Everything else the server
/// offers — `browser_run_code_unsafe`, network/storage tools, drag/drop, resize — is refused.
pub const MANAGED_TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_navigate_back",
    "browser_snapshot",
    "browser_find",
    "browser_click",
    "browser_type",
    "browser_fill_form",
    "browser_select_option",
    "browser_hover",
    "browser_press_key",
    "browser_wait_for",
    "browser_take_screenshot",
    "browser_tabs",
    "browser_handle_dialog",
    "browser_console_messages",
    "browser_evaluate",
    "browser_file_upload",
    "browser_close",
];

/// Tools of the extension backend (Browser MCP, v0.1.x).
pub const EXTENSION_TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_go_back",
    "browser_go_forward",
    "browser_snapshot",
    "browser_click",
    "browser_hover",
    "browser_type",
    "browser_select_option",
    "browser_press_key",
    "browser_wait",
    "browser_get_console_logs",
    "browser_screenshot",
];

/// Aqua-implemented tools available on the managed backend.
pub const AQUA_TOOLS: &[&str] = &["aqua_browser_extract", "aqua_browser_session"];

/// Tools that only read state and are safe to retry after a crash or transient failure.
pub fn is_idempotent(tool: &str) -> bool {
    matches!(
        tool,
        "browser_navigate"
            | "browser_navigate_back"
            | "browser_snapshot"
            | "browser_find"
            | "browser_wait_for"
            | "browser_take_screenshot"
            | "browser_console_messages"
            | "aqua_browser_extract"
            | "aqua_browser_session"
    ) || (tool == "browser_tabs")
}

/// Drop arguments the model must not control and apply safe defaults.
pub fn sanitize_args(tool: &str, mut args: Value) -> Value {
    if !args.is_object() {
        args = Value::Object(Default::default());
    }
    let obj = args.as_object_mut().expect("object");
    // Files are always auto-named inside Aqua's artifact directory.
    obj.remove("filename");
    match tool {
        "browser_take_screenshot" => {
            obj.entry("scale").or_insert_with(|| Value::String("css".into()));
            obj.entry("type").or_insert_with(|| Value::String("png".into()));
        }
        "browser_wait_for" => {
            if let Some(t) = obj.get("time").and_then(|v| v.as_f64()) {
                obj.insert("time".into(), Value::from(t.clamp(0.0, 30.0)));
            }
        }
        _ => {}
    }
    args
}

/// Minimal required-argument check against the pinned server's schemas
/// (`core/tests/fixtures/playwright_mcp/tools_list.json`).
/// Is this a ref from a page snapshot (`e55`, or `f1e55` for an element in a frame)? A target is one; a name such as `post_text_area` is not.
pub fn is_snapshot_ref(target: &str) -> bool {
    let t = target.trim();
    let rest = match t.strip_prefix('f') {
        Some(after) => match after.find('e') {
            Some(i) if i > 0 && after[..i].bytes().all(|b| b.is_ascii_digit()) => &after[i..],
            _ => return false,
        },
        None => t,
    };
    rest.strip_prefix('e').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Text that stands in for something the person never gave (an address like `your_email@example.com`, `<your name>`): typing it into a page is making things up.
pub fn looks_like_placeholder_text(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    if t.is_empty() {
        return false;
    }
    let example_address = t.rsplit_once('@').is_some_and(|(_, host)| ["example.com", "example.org", "example.net", "domain.com", "email.com"].contains(&host.trim_end_matches('.')));
    let stand_in = (t.starts_with("your_") || t.starts_with("your-") || t.starts_with("<your") || t.starts_with("[your") || t.starts_with("{your") || t.starts_with("{{"))
        && t.chars().count() < 60;
    example_address || stand_in || matches!(t.as_str(), "your email" | "your name" | "your password" | "password123" | "yourpassword" | "username" | "your username")
}

pub fn validate_args(tool: &str, args: &Value) -> std::result::Result<(), String> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty());
    let need = |k: &str| s(k).map(|_| ()).ok_or_else(|| format!("{tool} requires string \"{k}\""));
    match tool {
        "browser_navigate" => need("url"),
        "browser_click" | "browser_hover" => need("target"),
        "browser_type" => {
            need("target")?;
            match args.get("text").and_then(|v| v.as_str()) {
                Some(t) if looks_like_placeholder_text(t) => Err("browser_type was given placeholder text, not something the person gave (a made-up address or name): do not type it. Ask the person, or stop and say what is missing".into()),
                Some(t) if t.chars().count() <= 10_000 => Ok(()),
                Some(_) => Err("text is longer than 10000 characters".into()),
                None => Err("browser_type requires string \"text\"".into()),
            }
        }
        "browser_select_option" => {
            need("target")?;
            match args.get("values").and_then(|v| v.as_array()) {
                Some(v) if !v.is_empty() => Ok(()),
                _ => Err("browser_select_option requires non-empty \"values\"".into()),
            }
        }
        "browser_fill_form" => match args.get("fields").and_then(|v| v.as_array()) {
            Some(f) if (1..=30).contains(&f.len()) => {
                for field in f {
                    for k in ["target", "name", "type", "value"] {
                        if field.get(k).and_then(|v| v.as_str()).is_none() {
                            return Err(format!("each field needs string \"{k}\""));
                        }
                    }
                }
                Ok(())
            }
            _ => Err("browser_fill_form requires 1-30 \"fields\"".into()),
        },
        "browser_press_key" => need("key"),
        "browser_find" => match (s("text"), s("regex")) {
            (Some(_), None) => Ok(()),
            (None, Some(r)) if r.len() <= 200 => Ok(()),
            (None, Some(_)) => Err("regex is longer than 200 characters".into()),
            _ => Err("browser_find needs exactly one of \"text\" or \"regex\"".into()),
        },
        "browser_wait_for" => {
            if s("text").is_some() || s("textGone").is_some() || args.get("time").is_some() {
                Ok(())
            } else {
                Err("browser_wait_for needs \"text\", \"textGone\" or \"time\"".into())
            }
        }
        "browser_tabs" => match s("action") {
            Some("list" | "new") => Ok(()),
            Some("select" | "close") => args
                .get("index")
                .and_then(|v| v.as_u64())
                .map(|_| ())
                .ok_or_else(|| "browser_tabs select/close requires \"index\"".into()),
            _ => Err("browser_tabs \"action\" must be list, new, select or close".into()),
        },
        "browser_handle_dialog" => args
            .get("accept")
            .and_then(|v| v.as_bool())
            .map(|_| ())
            .ok_or_else(|| "browser_handle_dialog requires boolean \"accept\"".into()),
        "browser_console_messages" => match s("level") {
            Some("error" | "warning" | "info" | "debug") => Ok(()),
            _ => Err("level must be error, warning, info or debug".into()),
        },
        "browser_evaluate" => match s("function") {
            Some(f) if f.len() <= 4000 => Ok(()),
            Some(_) => Err("function is longer than 4000 characters".into()),
            None => Err("browser_evaluate requires string \"function\"".into()),
        },
        "browser_file_upload" => match args.get("paths").and_then(|v| v.as_array()) {
            Some(p) if !p.is_empty() && p.iter().all(|x| x.is_string()) => Ok(()),
            _ => Err("browser_file_upload requires \"paths\" (array of strings)".into()),
        },
        _ => Ok(()),
    }
}

/// Words that make a click or key press irreversible (sending, paying, deleting, posting…).
const IRREVERSIBLE_WORDS: &[&str] = &[
    "send", "submit", "buy", "purchase", "pay", "checkout", "order", "delete", "remove",
    "publish", "post", "tweet", "reply", "transfer", "confirm", "unsubscribe", "donate",
    "subscribe", "book", "reserve", "apply", "withdraw",
];

/// Field labels whose contents must never be typed without the user's approval.
const SENSITIVE_FIELD_WORDS: &[&str] = &[
    "password", "passcode", "passphrase", "pin", "otp", "cvv", "cvc", "ssn", "iban",
];
const SENSITIVE_FIELD_PHRASES: &[&str] = &[
    "card number", "credit card", "debit card", "security code", "one-time code",
    "verification code", "account number",
];

fn words(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// `label` describes an element whose activation can't be undone (e.g. button "Send").
pub fn is_irreversible_label(label: &str) -> bool {
    let w = words(label);
    w.iter().any(|x| IRREVERSIBLE_WORDS.contains(&x.as_str()))
        || label.to_ascii_lowercase().contains("place order")
}

/// `label` describes a credential or payment field.
pub fn is_sensitive_field_label(label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    words(label).iter().any(|x| SENSITIVE_FIELD_WORDS.contains(&x.as_str()))
        || SENSITIVE_FIELD_PHRASES.iter().any(|p| lower.contains(p))
}

/// Key presses that typically send or submit (e.g. Ctrl+Enter in mail and chat apps).
pub fn is_submit_shortcut(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace(' ', "");
    k.ends_with("+enter") && (k.contains("control") || k.contains("ctrl") || k.contains("meta"))
}

/// Resolve upload paths; each must be an existing file inside `uploads_dir`.
pub fn resolve_upload_paths(uploads_dir: &Path, paths: &[String]) -> std::result::Result<Vec<PathBuf>, String> {
    let root = uploads_dir
        .canonicalize()
        .map_err(|e| format!("uploads folder {} unavailable: {e}", uploads_dir.display()))?;
    paths
        .iter()
        .map(|p| {
            let candidate = Path::new(p);
            let joined = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                root.join(candidate.file_name().map(PathBuf::from).unwrap_or_default())
            };
            let real = joined
                .canonicalize()
                .map_err(|_| format!("upload file not found: {p}"))?;
            if !real.starts_with(&root) || !real.is_file() {
                return Err(format!(
                    "{p} is outside {} — copy the file there first",
                    uploads_dir.display()
                ));
            }
            Ok(real)
        })
        .collect()
}

#[cfg(test)]
mod resolving_tests {
    //! Audit item SE-09: a name is judged by the addresses it resolves to, not by its spelling. The resolver is a fixed table, so no test touches the network.
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Answers from a table; a name that is not in it does not resolve.
    struct Table(Vec<(&'static str, Vec<&'static str>)>);

    impl Resolver for Table {
        fn lookup(&self, host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
            match self.0.iter().find(|(name, _)| *name == host) {
                Some((_, ips)) => Ok(ips.iter().map(|ip| ip.parse().unwrap()).collect()),
                None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such name")),
            }
        }
    }

    /// A name that answers differently each time it is asked (a DNS-rebinding name): the answers in the order given, the last one for ever after.
    struct Flipping(Mutex<VecDeque<&'static str>>);

    impl Resolver for Flipping {
        fn lookup(&self, _host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
            let mut q = self.0.lock().unwrap();
            let ip = if q.len() > 1 { q.pop_front().unwrap() } else { q[0] };
            Ok(vec![ip.parse().unwrap()])
        }
    }

    /// Panics when asked: for what must never be looked up.
    struct Never;

    impl Resolver for Never {
        fn lookup(&self, host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
            panic!("{host} must not be looked up");
        }
    }

    fn deny_reason(d: UrlDecision) -> String {
        match d {
            UrlDecision::Deny(why) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn localtest_me_resolving_to_loopback_is_refused() {
        let table = Table(vec![("app.localtest.me", vec!["127.0.0.1"]), ("127.0.0.1.nip.io", vec!["127.0.0.1"]), ("ollama.example.org", vec!["::1"])]);
        let p = BrowserPolicyConfig::default();
        for url in ["http://app.localtest.me:2375/", "http://127.0.0.1.nip.io:11434/", "https://ollama.example.org/"] {
            // By name alone they pass as public hosts: the standard mode would ask, full access would open them without a question.
            assert!(matches!(p.check_url_with(url, false), UrlDecision::Ask { .. }), "{url}");
            assert_eq!(p.check_url_with(url, true), UrlDecision::Allow, "{url}");
            // With the look-up, both modes refuse, and say why.
            for full_access in [false, true] {
                let why = deny_reason(p.check_url_resolving(url, full_access, &table));
                assert!(why.contains("resolves to") && why.contains("allow_private"), "{url} full_access={full_access}: {why}");
            }
        }
        // The same in the person's allowlist and denylist modes.
        for mode in [SiteMode::Allowlist, SiteMode::Denylist] {
            let mut p = BrowserPolicyConfig::default();
            p.mode = mode;
            p.allow.push("app.localtest.me".into());
            assert!(matches!(p.check_url_resolving("http://app.localtest.me/", true, &table), UrlDecision::Deny(_)), "{mode:?}: being on the allow list does not say where the name may point");
        }
    }

    #[test]
    fn a_name_that_leads_to_aquas_own_services_is_refused_whatever_the_person_allowed() {
        let table = Table(vec![("gw.localtest.me", vec!["127.0.0.1"]), ("ui.example.org", vec!["::ffff:127.0.0.1"])]);
        let mut p = BrowserPolicyConfig::default();
        p.allow_private = vec!["gw.localtest.me".into(), "ui.example.org".into()];
        p.deny.clear();
        for url in ["http://gw.localtest.me:8767/api/v1/status", "http://gw.localtest.me:1420/", "http://ui.example.org:8766/"] {
            let why = deny_reason(p.check_url_resolving(url, true, &table));
            assert!(why.contains("Aqua's own"), "{url}: {why}");
        }
        // Another port on this computer is the person's to name in allow_private.
        assert_eq!(p.check_url_resolving("http://gw.localtest.me:5173/", true, &table), UrlDecision::Allow);
    }

    #[test]
    fn a_name_on_the_persons_own_network_opens_only_when_allow_private_names_it_and_no_deny_rule_covers_the_address() {
        let table = Table(vec![("nas.lan", vec!["192.168.1.50"]), ("printer.lan", vec!["192.168.1.60"]), ("db.example.org", vec!["10.1.2.3"])]);
        let mut p = BrowserPolicyConfig::default();
        // The default deny list covers 192.168.0.0/16: the answer names that rule, named in allow_private or not (deny always wins).
        assert!(deny_reason(p.check_url_resolving("http://nas.lan/", false, &table)).contains("192.168.0.0/16"));
        p.allow_private = vec!["nas.lan".into()];
        let why = deny_reason(p.check_url_resolving("http://nas.lan/", false, &table));
        assert!(why.contains("192.168.0.0/16"), "{why}");
        // The person takes the range off the deny list (as they would for an address typed as a number). A name not named is still refused, and the answer says what to do.
        p.deny.retain(|r| r != "192.168.0.0/16");
        assert!(deny_reason(p.check_url_resolving("http://printer.lan/", false, &table)).contains("allow_private"));
        // The named name is no longer refused, and only that name.
        assert!(matches!(p.check_url_resolving("http://nas.lan/", false, &table), UrlDecision::Ask { .. }), "named for its address, not allowed as a site: the site question stays");
        p.allow.push("nas.lan".into());
        assert_eq!(p.check_url_resolving("http://nas.lan/", false, &table), UrlDecision::Allow);
        assert!(matches!(p.check_url_resolving("http://printer.lan/", false, &table), UrlDecision::Deny(_)), "a second name on the same network is not named");
        // Being on the allow list is not naming it private.
        p.allow.push("db.example.org".into());
        p.deny.retain(|r| r != "10.0.0.0/8");
        assert!(deny_reason(p.check_url_resolving("http://db.example.org/", false, &table)).contains("allow_private"));
        // A port in the rule limits it to that port.
        p.allow_private = vec!["nas.lan:8080".into()];
        assert_eq!(p.check_url_resolving("http://nas.lan:8080/", false, &table), UrlDecision::Allow);
        assert!(matches!(p.check_url_resolving("http://nas.lan:9090/", false, &table), UrlDecision::Deny(_)));
    }

    #[test]
    fn one_private_answer_in_a_mix_refuses_the_whole_name() {
        let table = Table(vec![("mixed.example.org", vec!["93.184.216.34", "169.254.169.254"]), ("v6.example.org", vec!["2606:4700:4700::1111", "fd00::5"])]);
        let p = BrowserPolicyConfig::default();
        assert!(matches!(p.check_url_resolving("https://mixed.example.org/", true, &table), UrlDecision::Deny(_)));
        assert!(matches!(p.check_url_resolving("https://v6.example.org/", true, &table), UrlDecision::Deny(_)));
    }

    #[test]
    fn answers_written_as_ipv4_mapped_ipv6_are_judged_as_the_ipv4_address() {
        let table = Table(vec![("map.example.org", vec!["::ffff:10.0.0.5"]), ("pub.example.org", vec!["::ffff:93.184.216.34"])]);
        let p = BrowserPolicyConfig::default();
        let why = deny_reason(p.check_url_resolving("https://map.example.org/", true, &table));
        assert!(why.contains("10.0.0.5") && why.contains("10.0.0.0/8"), "the CIDR rules see the IPv4 form: {why}");
        assert!(matches!(p.check_url_resolving("https://pub.example.org/", false, &table), UrlDecision::Ask { .. }));
    }

    #[test]
    fn a_deny_rule_that_names_a_resolved_address_wins_over_a_public_name() {
        let table = Table(vec![
            ("blocked.example.org", vec!["93.184.216.34"]),
            ("fine.example.org", vec!["93.184.216.35"]),
            ("blocked6.example.org", vec!["2606:4700:4700::1111"]),
        ]);
        let mut p = BrowserPolicyConfig::default();
        p.allow.push("blocked.example.org".into());
        p.allow.push("blocked6.example.org".into());
        p.allow.push("fine.example.org".into());
        p.deny.push("93.184.216.34".into());
        p.deny.push("[2606:4700:4700::1111]".into());
        assert!(deny_reason(p.check_url_resolving("https://blocked.example.org/", false, &table)).contains("93.184.216.34"));
        assert!(deny_reason(p.check_url_resolving("https://blocked6.example.org/", false, &table)).contains("2606:4700:4700::1111"));
        assert_eq!(p.check_url_resolving("https://fine.example.org/", false, &table), UrlDecision::Allow);
    }

    #[test]
    fn public_names_are_decided_as_before_and_what_was_refused_stays_refused() {
        let table = Table(vec![("www.bbc.com", vec!["151.101.0.81"]), ("example.org", vec!["93.184.216.34"]), ("evil.example", vec!["93.184.216.36"])]);
        let mut p = BrowserPolicyConfig::default();
        p.deny.push("evil.example".into());
        for (url, full_access) in [
            ("https://www.bbc.com/news", false),
            ("https://example.org/", false),
            ("https://example.org/", true),
            ("https://evil.example/", true),
            ("http://192.168.1.1/", false),
            ("file:///C:/x", true),
            ("about:blank", false),
            ("not a url", false),
        ] {
            assert_eq!(p.check_url_resolving(url, full_access, &table), p.check_url_with(url, full_access), "{url} full_access={full_access}");
        }
    }

    #[test]
    fn a_name_that_does_not_resolve_is_an_unknown_host_and_asks_where_full_access_would_have_opened_it() {
        let nothing = Table(Vec::new());
        let mut p = BrowserPolicyConfig::default();
        // Not listed: the question stays a question under full access (offline, or a name nobody can look up).
        assert!(matches!(p.check_url_resolving("https://gone.example.org/", true, &nothing), UrlDecision::Ask { .. }));
        assert!(matches!(p.check_url_resolving("https://gone.example.org/", false, &nothing), UrlDecision::Ask { .. }));
        // Listed by the person: as decided (the page will not load; the look-up after loading looks again).
        assert_eq!(p.check_url_resolving("https://en.wikipedia.org/wiki/Aqua", true, &nothing), UrlDecision::Allow);
        // The person's denylist mode: as decided. The allowlist mode: refused, as before.
        p.mode = SiteMode::Denylist;
        assert_eq!(p.check_url_resolving("https://gone.example.org/", true, &nothing), UrlDecision::Allow);
        p.mode = SiteMode::Allowlist;
        assert!(matches!(p.check_url_resolving("https://gone.example.org/", true, &nothing), UrlDecision::Deny(_)));
    }

    #[test]
    fn numbers_localhost_and_a_blank_page_are_not_looked_up() {
        let p = BrowserPolicyConfig::default();
        for url in ["http://127.0.0.1:8791/news.html", "http://localhost:5173/", "http://LOCALHOST.:5173/", "http://app.localhost/", "about:blank", "http://[2606:4700:4700::1111]/", "http://192.168.1.1/admin", "http://localhost:8767/"] {
            assert_eq!(p.check_url_resolving(url, false, &Never), p.check_url_with(url, false), "{url}");
        }
    }

    #[test]
    fn a_rebinding_name_that_answers_public_first_and_private_after_is_refused_by_the_second_look_up() {
        let mut p = BrowserPolicyConfig::default();
        p.allow.push("rebind.example.org".into());
        let flip = Flipping(Mutex::new(VecDeque::from(["93.184.216.34", "127.0.0.1"])));
        let url = "http://rebind.example.org:2375/";
        assert_eq!(p.check_url_resolving(url, false, &flip), UrlDecision::Allow, "the first answer is public");
        assert!(matches!(p.check_url_resolving(url, false, &flip), UrlDecision::Deny(_)), "the next look-up (after the page has loaded) is not");
    }

    #[test]
    fn the_new_rule_list_is_optional_in_a_saved_policy_and_empty_rules_are_rejected() {
        // A policy file written before allow_private existed still loads, with the list empty.
        let mut older = serde_json::to_value(BrowserPolicyConfig::default()).unwrap();
        older.as_object_mut().unwrap().remove("allow_private");
        let loaded: BrowserPolicyConfig = serde_json::from_value(older).unwrap();
        assert!(loaded.allow_private.is_empty());
        assert_eq!(loaded, BrowserPolicyConfig::default());
        let mut p = BrowserPolicyConfig::default();
        p.allow_private = vec!["nas.lan".into(), "  ".into()];
        assert!(p.validate().is_err());
        p.allow_private = vec!["nas.lan".into()];
        assert!(p.validate().is_ok());
    }

    #[test]
    fn the_computers_own_resolver_answers_for_localhost_and_for_a_number() {
        let found = SystemResolver.lookup("localhost", 80).expect("localhost resolves on every computer");
        assert!(!found.is_empty() && found.iter().all(|ip| is_non_public(*ip)), "{found:?}");
        assert_eq!(SystemResolver.lookup("127.0.0.1", 80).unwrap(), vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
    }
}

#[cfg(test)]
mod one_door_tests {
    use super::*;

    #[test]
    fn a_site_nobody_listed_opens_without_a_question_only_under_full_access_and_that_is_what_this_names() {
        let mut p = BrowserPolicyConfig::default();
        p.allow = vec!["docs.example.org".into()];
        p.deny = vec!["bad.example.net".into()];
        // Not listed, and the policy would ask: under full access it opens silently. That is the case.
        assert!(p.opens_silently_only_by_full_access("https://collector.example.net/?d=my+notes", true));
        assert!(p.opens_silently_only_by_full_access("https://x.collector.example.net/", true), "a host that carries data is as unlisted as any");
        // Without full access the site prompt asks anyway: nothing silent.
        assert!(!p.opens_silently_only_by_full_access("https://collector.example.net/?d=my+notes", false));
        // Listed, denied, private, blank and not a web address: not this.
        for not_this in ["https://docs.example.org/page", "https://bad.example.net/", "http://127.0.0.1:8080/", "about:blank", "javascript:alert(1)", "not a url"] {
            assert!(!p.opens_silently_only_by_full_access(not_this, true), "{not_this}");
        }
        // The person chose to open everything but what is denied: that is their setting, not the switch's doing.
        p.mode = SiteMode::Denylist;
        assert!(!p.opens_silently_only_by_full_access("https://collector.example.net/", true));
    }

    /// Card R6-08: a run that read outside text asks about an address nobody listed in every site mode, so the question is "does it open with no question and is it listed",
    /// not "did full access cause it".
    #[test]
    fn a_site_nobody_listed_that_opens_with_no_question_is_named_whether_full_access_or_the_denylist_mode_lets_it() {
        let mut p = BrowserPolicyConfig::default();
        p.allow = vec!["docs.example.org".into()];
        p.deny = vec!["bad.example.net".into()];
        let leak = "https://collector.example.net/?d=my+notes";
        // Ask mode: only full access opens it silently.
        assert!(p.opens_unlisted_without_a_question(leak, true) && !p.opens_unlisted_without_a_question(leak, false));
        // The person's denylist mode opens everything not denied, with or without full access: that is now named too.
        p.mode = SiteMode::Denylist;
        assert!(p.opens_unlisted_without_a_question(leak, false) && p.opens_unlisted_without_a_question(leak, true));
        assert!(!p.opens_silently_only_by_full_access(leak, true), "the narrower name is unchanged");
        // Never named: a listed site, a denied one, a private address, a blank page, something that is not a web address.
        for not_this in ["https://docs.example.org/page?q=x", "https://bad.example.net/", "http://127.0.0.1:8080/", "about:blank", "javascript:alert(1)", "not a url"] {
            for full in [false, true] {
                assert!(!p.opens_unlisted_without_a_question(not_this, full), "{not_this} full={full}");
            }
        }
        // The allowlist mode refuses what is not listed: nothing opens, so nothing opens silently.
        p.mode = SiteMode::Allowlist;
        assert!(!p.opens_unlisted_without_a_question(leak, true));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_target_is_a_ref_from_the_snapshot_and_placeholder_text_is_not_something_to_type() {
        use super::{is_snapshot_ref, looks_like_placeholder_text};
        for yes in ["e55", "e1", "f1e55", "f12e340", " e7 "] {
            assert!(is_snapshot_ref(yes), "{yes}");
        }
        for no in ["post_text_area", "e", "f1", "fe5", "ref=e55", "#post", "e5x", "", "https://www.facebook.com/"] {
            assert!(!is_snapshot_ref(no), "{no}");
        }
        for placeholder in ["your_email@example.com", "user@example.com", "<your name>", "{{password}}", "your password", "Password123"] {
            assert!(looks_like_placeholder_text(placeholder), "{placeholder}");
        }
        for real in ["Hello from Aqua", "anna@gmail.com", "I am testing my own assistant", "", "example.com is a reserved domain"] {
            assert!(!looks_like_placeholder_text(real), "{real}");
        }
        let err = super::validate_args("browser_type", &serde_json::json!({"target": "e55", "text": "your_email@example.com"})).unwrap_err();
        assert!(err.contains("placeholder text"), "{err}");
        assert!(super::validate_args("browser_type", &serde_json::json!({"target": "e55", "text": "a real sentence"})).is_ok());
    }

    #[test]
    fn a_request_names_the_browser_it_wants_and_other_uses_of_the_words_name_none() {
        use super::{requested_channel, BrowserChannel::{Chrome, Msedge}};
        for (text, want) in [
            ("in chrome browser go to Facebook and post a post related to ai aqua can do", Chrome),
            ("in chrome go to facebook", Chrome),
            ("open chrome go to facebook and post", Chrome),
            ("go to facebook in Chrome", Chrome),
            ("Open Google Chrome and search for rust", Chrome),
            ("use my Chrome browser to read the page", Chrome),
            ("using Edge open the news", Msedge),
            ("in Microsoft Edge go to bbc.com", Msedge),
            ("open the page with msedge", Msedge),
            ("the Edge browser is where I want it", Msedge),
        ] {
            assert_eq!(requested_channel(text), Some(want), "{text}");
        }
        for text in [
            "tell me about the cutting edge of AI",
            "she stood on the edge of the cliff",
            "search my pc for chrome",
            "install the chrome extension",
            "use chrome and edge side by side",
            "go to facebook and post",
            "",
        ] {
            assert_eq!(requested_channel(text), None, "{text}");
        }
        assert_eq!(super::BrowserChannel::parse("Google Chrome"), Some(Chrome));
        assert_eq!(super::BrowserChannel::parse(" msedge "), Some(Msedge));
        assert_eq!(super::BrowserChannel::parse("firefox"), None);
    }

    #[test]
    fn local_addresses_are_recognised_however_they_are_written() {
        let p = BrowserPolicyConfig::default();
        // Aqua's own services, written as IPv4-mapped IPv6 or with a trailing dot, are still Aqua's own services
        for url in ["http://[::ffff:127.0.0.1]:8767/", "http://[::ffff:7f00:1]:8767/", "http://localhost.:8767/", "http://LOCALHOST.:1420/", "http://127.0.0.1:8766/"] {
            assert!(matches!(p.check_url(url), UrlDecision::Deny(ref why) if why.contains("Aqua's own")), "{url}: {:?}", p.check_url(url));
        }
        // a private address written as IPv6 is judged as the private IPv4 address
        let strict = BrowserPolicyConfig { allow: vec![], ..BrowserPolicyConfig::default() };
        for url in ["http://[::ffff:10.0.0.5]/", "http://[::ffff:192.168.1.1]/admin", "http://[::ffff:169.254.169.254]/latest/meta-data"] {
            assert!(matches!(strict.check_url(url), UrlDecision::Deny(_)), "{url}");
        }
        // ordinary public sites are untouched
        assert!(matches!(strict.check_url("https://example.org/"), UrlDecision::Ask { .. }));
        assert!(matches!(p.check_url("https://en.wikipedia.org/wiki/Aqua"), UrlDecision::Allow));
    }

    #[test]
    fn full_access_opens_unknown_sites_without_asking_and_changes_no_refusal() {
        let mut ask = BrowserPolicyConfig::default();
        ask.mode = SiteMode::Ask;
        assert!(matches!(ask.check_url_with("https://example.org/", false), UrlDecision::Ask { .. }));
        assert_eq!(ask.check_url_with("https://example.org/", true), UrlDecision::Allow);
        // Refusals are decided before the mode and stay refused.
        for refused in ["http://localhost/", "http://192.168.0.1/", "http://127.0.0.1:8767/api/v1/health", "file:///C:/Windows/win.ini", "javascript:alert(1)", "http://127.0.0.1:1420/"] {
            if let UrlDecision::Allow = ask.check_url_with(refused, true) {
                assert!(ask.allow.iter().any(|r| refused.contains(r.as_str())), "{refused} must not open under full access");
            }
        }
        assert!(matches!(ask.check_url_with("http://127.0.0.1:8767/api/v1/health", true), UrlDecision::Deny(_)), "Aqua's own services");
        assert!(matches!(ask.check_url_with("http://192.168.0.1/", true), UrlDecision::Deny(_)), "private addresses");
        assert!(matches!(ask.check_url_with("file:///C:/x", true), UrlDecision::Deny(_)), "other schemes");
        // A deny rule still wins, and an allowlist-only policy still refuses what is not listed.
        ask.deny.push("evil.example".into());
        assert!(matches!(ask.check_url_with("https://evil.example/", true), UrlDecision::Deny(_)));
        let mut only = BrowserPolicyConfig::default();
        only.mode = SiteMode::Allowlist;
        assert!(matches!(only.check_url_with("https://example.org/", true), UrlDecision::Deny(_)));
    }

    #[test]
    fn the_local_machine_check_covers_names_numbers_and_the_ipv6_disguise() {
        for local in ["http://localhost/", "http://localhost.:5678/", "http://db.localhost/", "http://127.0.0.1:5678/", "http://[::1]/", "http://[::ffff:127.0.0.1]/", "http://10.1.2.3/", "http://192.168.0.1/", "http://2130706433/", "http://0x7f.0.0.1/", "http://intranet/"] {
            assert!(url_targets_local_machine(local), "{local}");
        }
        for public in ["https://example.org/", "https://en.wikipedia.org/", "http://93.184.216.34/", "https://localhost.example.com/"] {
            assert!(!url_targets_local_machine(public), "{public}");
        }
        assert!(!url_targets_local_machine("not a url"));
    }

    use super::*;

    fn p() -> BrowserPolicyConfig {
        BrowserPolicyConfig::default()
    }

    #[test]
    fn allowlisted_sites_and_subdomains_pass() {
        assert_eq!(p().check_url("https://www.bbc.com/news"), UrlDecision::Allow);
        assert_eq!(p().check_url("https://bbc.com/"), UrlDecision::Allow);
        assert_eq!(p().check_url("http://127.0.0.1:8791/news.html"), UrlDecision::Allow);
        assert_eq!(p().check_url("about:blank"), UrlDecision::Allow);
    }

    #[test]
    fn unknown_sites_ask_in_ask_mode() {
        match p().check_url("https://example.org/page") {
            UrlDecision::Ask { site, origin } => {
                assert_eq!(site, "example.org");
                assert_eq!(origin, "https://example.org");
            }
            other => panic!("expected ask, got {other:?}"),
        }
    }

    #[test]
    fn dangerous_targets_are_denied() {
        for url in [
            "file:///C:/Windows/System32/drivers/etc/hosts",
            "javascript:alert(1)",
            "data:text/html,hi",
            "chrome://settings",
            "edge://settings",
            "http://192.168.1.1/admin",
            "http://10.1.2.3/",
            "http://169.254.169.254/latest/meta-data",
            "http://127.0.0.1:8767/api/v1/status",
            "http://localhost:8766/",
            "http://[::1]:8767/",
            "http://127.0.0.1:1420/",
            "http://localhost:1420/api/v1/browser/confirm",
            "http://[::1]:1420/",
            "http://0.0.0.0:8767/",
            "http://app.localhost:1420/",
            "http://[fe80::1]/",
            "not a url",
        ] {
            assert!(matches!(p().check_url(url), UrlDecision::Deny(_)), "{url} should be denied");
        }
    }

    #[test]
    fn aquas_own_ports_stay_blocked_even_if_the_policy_file_allows_or_forgets_them() {
        let mut cfg = p();
        cfg.deny.clear();
        cfg.allow = vec!["localhost".into(), "127.0.0.1".into(), "localhost:1420".into(), "127.0.0.1:8767".into(), "*".into()];
        for url in ["http://localhost:1420/", "http://127.0.0.1:1420/", "http://127.0.0.1:8767/api/v1/status", "http://localhost:8766/"] {
            assert!(matches!(cfg.check_url(url), UrlDecision::Deny(_)), "{url}");
        }
        // Other local dev servers are still the user's to allow.
        assert_eq!(cfg.check_url("http://localhost:5173/"), UrlDecision::Allow);
    }

    #[test]
    fn deny_wins_over_allow() {
        let mut cfg = p();
        cfg.allow.push("192.168.1.1".into());
        assert!(matches!(cfg.check_url("http://192.168.1.1/"), UrlDecision::Deny(_)));
    }

    #[test]
    fn private_ip_allowed_only_by_explicit_rule_when_not_denied() {
        let mut cfg = p();
        cfg.deny.retain(|r| r != "192.168.0.0/16");
        assert!(matches!(cfg.check_url("http://192.168.1.50/"), UrlDecision::Deny(_)));
        cfg.allow.push("192.168.1.50".into());
        assert_eq!(cfg.check_url("http://192.168.1.50/"), UrlDecision::Allow);
    }

    #[test]
    fn modes_change_unknown_site_handling() {
        let mut cfg = p();
        cfg.mode = SiteMode::Allowlist;
        assert!(matches!(cfg.check_url("https://example.org"), UrlDecision::Deny(_)));
        cfg.mode = SiteMode::Denylist;
        assert_eq!(cfg.check_url("https://example.org"), UrlDecision::Allow);
    }

    #[test]
    fn rules_support_ports_globs_and_schemes() {
        let mut cfg = p();
        cfg.mode = SiteMode::Allowlist;
        cfg.allow = vec!["https://docs.rs/".into(), "api-*.example.com".into(), "localhost:5173".into()];
        assert_eq!(cfg.check_url("https://docs.rs/url"), UrlDecision::Allow);
        assert_eq!(cfg.check_url("https://api-eu.example.com/v1"), UrlDecision::Allow);
        assert_eq!(cfg.check_url("http://localhost:5173/"), UrlDecision::Allow);
        assert!(matches!(cfg.check_url("http://localhost:3000/"), UrlDecision::Deny(_)));
        assert!(matches!(cfg.check_url("https://evil-docs.rs/"), UrlDecision::Deny(_)));
    }

    #[test]
    fn javascript_policy() {
        let mut cfg = p();
        assert!(!cfg.javascript_allowed(Some("https://www.bbc.com/")));
        cfg.javascript = JavascriptPolicy::AllowedSites;
        cfg.javascript_sites = vec!["*.bbc.com".into()];
        assert!(cfg.javascript_allowed(Some("https://www.bbc.com/news")));
        assert!(!cfg.javascript_allowed(Some("https://example.org/")));
        assert!(!cfg.javascript_allowed(None));
    }

    #[test]
    fn classifiers() {
        assert!(is_irreversible_label("button \"Send\""));
        assert!(is_irreversible_label("Place order"));
        assert!(is_irreversible_label("button \"Delete account\""));
        assert!(!is_irreversible_label("link \"Sender guidelines\""));
        assert!(!is_irreversible_label("button \"Search\""));
        assert!(!is_irreversible_label("button \"Sign in\""));
        assert!(!is_irreversible_label("button \"Cancel\""));
        assert!(is_sensitive_field_label("textbox \"Password\""));
        assert!(is_sensitive_field_label("Card number"));
        assert!(!is_sensitive_field_label("textbox \"Name\""));
        assert!(!is_sensitive_field_label("textbox \"Spinach recipe\"")); // "pin" is a whole word only
        assert!(is_submit_shortcut("Control+Enter"));
        assert!(!is_submit_shortcut("Enter"));
    }

    #[test]
    fn sanitize_strips_filename_and_sets_defaults() {
        let a = sanitize_args("browser_take_screenshot", serde_json::json!({"filename": "C:/x.png"}));
        assert!(a.get("filename").is_none());
        assert_eq!(a["scale"], "css");
        let w = sanitize_args("browser_wait_for", serde_json::json!({"time": 999}));
        assert_eq!(w["time"], 30.0);
    }

    #[test]
    fn validate_required_args() {
        assert!(validate_args("browser_navigate", &serde_json::json!({})).is_err());
        assert!(validate_args("browser_click", &serde_json::json!({"target": "f1e4"})).is_ok());
        assert!(validate_args("browser_find", &serde_json::json!({"text": "a", "regex": "b"})).is_err());
        assert!(validate_args("browser_tabs", &serde_json::json!({"action": "close"})).is_err());
        assert!(validate_args(
            "browser_fill_form",
            &serde_json::json!({"fields": [{"target": "f1e4", "name": "Name", "type": "textbox", "value": "x"}]})
        )
        .is_ok());
    }

    #[test]
    fn uploads_confined_to_folder() {
        let dir = std::env::temp_dir().join(format!("aqua_upload_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.txt"), b"x").unwrap();
        assert!(resolve_upload_paths(&dir, &["ok.txt".into()]).is_ok());
        assert!(resolve_upload_paths(&dir, &["missing.txt".into()]).is_err());
        let outside = std::env::temp_dir().join(format!("aqua_outside_{}.txt", std::process::id()));
        std::fs::write(&outside, b"secret").unwrap();
        assert!(resolve_upload_paths(&dir, &[outside.display().to_string()]).is_err());
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn committed_default_policy_matches_code_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/browser/policy.default.json");
        let file: BrowserPolicyConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(file, BrowserPolicyConfig::default());
    }

    #[test]
    fn policy_file_round_trip_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("aqua_policy_test_{}", std::process::id()));
        let path = dir.join("policy.json");
        let _ = std::fs::remove_dir_all(&dir);
        let created = BrowserPolicyConfig::load_or_create(&path).unwrap();
        assert_eq!(created, BrowserPolicyConfig::default());
        assert_eq!(BrowserPolicyConfig::load_or_create(&path).unwrap(), created);
        std::fs::write(&path, b"{not json").unwrap();
        assert!(BrowserPolicyConfig::load_or_create(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
