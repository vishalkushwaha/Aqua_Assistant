//! Where the HTTP tools may connect.
//!
//! Every request, and every redirect hop, is checked here before a connection is made:
//! - only `http` and `https`, no user name or password in the URL;
//! - never Aqua's own services, never a service port such as SSH, SMTP, databases or Docker;
//! - never a private, loopback, link-local, cloud-metadata or otherwise non-public address, unless
//!   the config names that exact host and port in `allow_private` (a local dev server, say);
//! - a host name is resolved by Aqua, and the connection goes only to the addresses it vetted, so
//!   a name that later resolves somewhere else (DNS rebinding) cannot redirect the request.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// Ports of Aqua's own services (UI, voice daemon, API gateway).
pub const AQUA_PORTS: &[u16] = &[1420, 8766, 8767];

/// Service ports that have no business being reached from an agent's web request.
pub const DEFAULT_BLOCKED_PORTS: &[u16] = &[22, 23, 25, 110, 135, 139, 143, 445, 465, 587, 993, 995, 1433, 1521, 2375, 2376, 3306, 3389, 5432, 5900, 6379, 9200, 11211, 27017];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Bearer,
    Basic,
    Header,
}

/// A credential the model can use by name. The secret is stored in Windows Credential Manager
/// and is attached only to requests for the listed hosts; the model never sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthProfile {
    pub kind: AuthKind,
    /// Name in Credential Manager (stored as `Aqua/<secret>`).
    pub secret: String,
    /// Hosts the credential may be sent to (`api.github.com`, `*.example.com`).
    pub hosts: Vec<String>,
    /// For `basic`: the user name. For `header`: the header name (default `X-API-Key`).
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchEngine {
    Searxng,
    Brave,
    /// Tavily (an API key in an auth profile).
    Tavily,
    /// Wikipedia's own search: no key. The last resort when nothing else answers.
    Wikipedia,
    /// DuckDuckGo's results page: no key. Asked before Wikipedia when the person has set nothing, so that a question about a thing that has no article (a product, a
    /// small company, a new tool) is not answered with nothing. It is a page and not a service, and it answers with a check (HTTP 202) when it sees many automated
    /// requests: then the next service is asked.
    #[serde(rename = "duckduckgo")]
    DuckDuckGo,
    /// Parallel's search, with no key (`search.parallel.ai/mcp`, an MCP server spoken to over HTTP). **Only when the person lists it.** Parallel's customer terms (the ones the
    /// service's own answer points to) let it use what it is sent to train its models, and say personal data needs a separate agreement; nothing says how long queries are kept.
    /// The query filter holds back more for it than for the others (see `sensitive`).
    Parallel,
    /// Ollama's web search (`ollama.com/api/web_search`), with the key of a free account in an auth profile. **Only when the person lists it.** Ollama's privacy policy says its
    /// cloud models never train on prompts, and does not say what it does with a search query.
    Ollama,
}

impl SearchEngine {
    /// May this service keep or train on what it is sent, as far as its terms say or leave open? Then the query filter holds back more (see `sensitive::find`).
    pub fn may_keep_queries(self) -> bool {
        matches!(self, SearchEngine::Parallel | SearchEngine::Ollama)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    pub engine: SearchEngine,
    /// SearXNG base URL (`https://searx.example.org`). For the other services the address they have by default, when the person points it
    /// somewhere else (a proxy of their own).
    #[serde(default)]
    pub url: Option<String>,
    /// Auth profile holding the Brave API key.
    #[serde(default)]
    pub auth: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Hosts that need no prompt for reads: `example.com`, `*.example.com`, `example.com:8443`.
    #[serde(default = "default_allow")]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    /// Private or local hosts that are allowed on purpose, with their port (`127.0.0.1:5173`).
    #[serde(default)]
    pub allow_private: Vec<String>,
    #[serde(default)]
    pub blocked_ports: Vec<u16>,
    /// What to do with a host that is in neither list: ask, or refuse.
    #[serde(default = "default_unknown")]
    pub unknown_hosts: String,
    #[serde(default)]
    pub auth: BTreeMap<String, AuthProfile>,
    #[serde(default)]
    pub search: Option<SearchConfig>,
    /// Other search services, asked in this order when `search` finds nothing or fails. Wikipedia (no key) always comes last.
    #[serde(default)]
    pub search_fallback: Vec<SearchConfig>,
    /// `web.scrape` drives a real browser engine (page scripts run). On by default; set `"scrape": false` in
    /// data/tools/http.json to hide it from the model.
    #[serde(default = "default_scrape")]
    pub scrape: bool,
}

fn default_allow() -> Vec<String> {
    ["*.wikipedia.org", "*.bbc.com", "*.bbc.co.uk", "*.duckduckgo.com", "wttr.in", "docs.rs", "crates.io", "*.rust-lang.org"].iter().map(|s| s.to_string()).collect()
}

fn default_scrape() -> bool {
    true
}

fn default_unknown() -> String {
    "ask".into()
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self { allow: default_allow(), deny: Vec::new(), allow_private: Vec::new(), blocked_ports: Vec::new(), unknown_hosts: default_unknown(), auth: BTreeMap::new(), search: None, search_fallback: Vec::new(), scrape: true }
    }
}

impl HttpConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.unknown_hosts.as_str(), "ask" | "deny") {
            return Err("unknown_hosts must be 'ask' or 'deny'".into());
        }
        for (name, p) in &self.auth {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("auth profile '{name}' must be letters, digits, '_' or '-'"));
            }
            if p.hosts.is_empty() || p.hosts.iter().any(|h| h.trim().is_empty() || h == "*" || h.contains('/')) {
                return Err(format!("auth profile '{name}' must list specific hosts (not '*')"));
            }
            if p.secret.is_empty() || p.secret.len() > 100 || !p.secret.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.')) {
                return Err(format!("auth profile '{name}': secret name is not valid"));
            }
            if p.kind == AuthKind::Basic && p.name.as_deref().unwrap_or("").is_empty() {
                return Err(format!("auth profile '{name}': basic needs a user name in 'name'"));
            }
            if let Some(h) = &p.name {
                if p.kind == AuthKind::Header && (h.is_empty() || !h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
                    return Err(format!("auth profile '{name}': header name is not valid"));
                }
            }
        }
        for s in self.search.iter().chain(self.search_fallback.iter()) {
            match s.engine {
                SearchEngine::Searxng if s.url.as_deref().unwrap_or("").is_empty() => return Err("search engine searxng needs 'url'".into()),
                SearchEngine::Brave if s.auth.as_deref().map(|a| self.auth.contains_key(a)) != Some(true) => return Err("search engine brave needs 'auth' naming an auth profile".into()),
                SearchEngine::Tavily if s.auth.as_deref().map(|a| self.auth.contains_key(a)) != Some(true) => return Err("search engine tavily needs 'auth' naming an auth profile".into()),
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Not on the allow list: the user is asked.
    Ask,
    Deny(String),
}

#[derive(Debug, Clone)]
pub struct Target {
    pub url: url::Url,
    pub host: String,
    pub port: u16,
    pub ip: Option<IpAddr>,
}

pub struct NetPolicy {
    cfg: HttpConfig,
}

impl NetPolicy {
    pub fn new(cfg: HttpConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &HttpConfig {
        &self.cfg
    }

    pub fn parse(raw: &str) -> Result<Target, String> {
        let url = url::Url::parse(raw.trim()).map_err(|e| format!("not a valid URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("scheme '{}:' is not allowed; only http and https", url.scheme()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("a user name or password inside the URL is not allowed; use an auth profile".into());
        }
        let host = url.host().ok_or("the URL has no host")?;
        let (host_s, ip) = match host {
            // "localhost." and "localhost" are the same machine; so are "Example.COM." and "example.com"
            url::Host::Domain(d) => (d.trim_end_matches('.').to_ascii_lowercase(), None),
            url::Host::Ipv4(v4) => (v4.to_string(), Some(IpAddr::V4(v4))),
            url::Host::Ipv6(v6) => (format!("[{v6}]"), Some(IpAddr::V6(v6))),
        };
        let port = url.port_or_known_default().ok_or("the URL has no port")?;
        Ok(Target { url, host: host_s, port, ip })
    }

    fn matches(rule: &str, host: &str, port: u16) -> bool {
        let r = rule.trim().to_ascii_lowercase();
        // `host`, `host:port`, `[v6]` or `[v6]:port`.
        let (rh, rp): (String, Option<u16>) = if r.starts_with('[') {
            match r.split_once("]:") {
                Some((h, p)) => (format!("{h}]"), p.parse().ok()),
                None => (r.clone(), None),
            }
        } else {
            match r.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), p.parse().ok()),
                _ => (r.clone(), None),
            }
        };
        if let Some(p) = rp {
            if p != port {
                return false;
            }
        }
        if rh == host {
            return true;
        }
        rh.strip_prefix("*.").map(|d| host.ends_with(&format!(".{d}")) || host == d).unwrap_or(false)
    }

    fn explicitly_private(&self, t: &Target) -> bool {
        self.cfg.allow_private.iter().any(|r| Self::matches(r, &t.host, t.port))
    }

    pub fn check(&self, t: &Target) -> Verdict {
        let loopback_host = t.host == "localhost" || t.host.ends_with(".localhost") || t.ip.map(|i| is_loopback_or_unspecified(i)).unwrap_or(false);
        if loopback_host && AQUA_PORTS.contains(&t.port) {
            return Verdict::Deny(format!("{}:{} is one of Aqua's own local services", t.host, t.port));
        }
        if DEFAULT_BLOCKED_PORTS.contains(&t.port) || self.cfg.blocked_ports.contains(&t.port) {
            return Verdict::Deny(format!("port {} is a service port that web requests may not reach", t.port));
        }
        if let Some(rule) = self.cfg.deny.iter().find(|r| Self::matches(r, &t.host, t.port)) {
            return Verdict::Deny(format!("{}:{} matches deny rule \"{rule}\"", t.host, t.port));
        }
        let local_name = t.host == "localhost" || [".localhost", ".local", ".internal", ".lan", ".home.arpa", ".corp", ".intranet"].iter().any(|s| t.host.ends_with(s)) || !t.host.contains('.') && t.ip.is_none();
        let private = t.ip.map(is_non_public).unwrap_or(local_name);
        if private {
            return if self.explicitly_private(t) { Verdict::Allow } else { Verdict::Deny(format!("{} is a private or local address; add \"{}:{}\" to allow_private if that is intended", t.host, t.host, t.port)) };
        }
        if self.cfg.allow.iter().any(|r| Self::matches(r, &t.host, t.port)) {
            return Verdict::Allow;
        }
        if self.cfg.unknown_hosts == "deny" {
            return Verdict::Deny(format!("{} is not on the allow list", t.host));
        }
        Verdict::Ask
    }

    /// May a credential travel to this target? Only over https, or to this computer itself: over plain http a Bearer
    /// token or password can be read by anyone on the path (a shared network, a captive portal).
    pub fn credential_transport_ok(t: &Target) -> bool {
        t.url.scheme() == "https" || t.host == "localhost" || t.host.ends_with(".localhost") || t.ip.is_some_and(is_loopback_or_unspecified)
    }

    /// The host is one an auth profile may be sent to.
    pub fn profile_allows(&self, profile: &AuthProfile, host: &str, port: u16) -> bool {
        profile.hosts.iter().any(|r| Self::matches(r, host, port))
    }

    /// Resolve `host:port` and return only addresses the policy allows. If any answer is non-public and
    /// not explicitly allowed the whole name is refused, so a mix of good and bad answers cannot be used.
    pub fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let denied = |m: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, m);
        let (host, port) = match netloc.rsplit_once(':') {
            Some((h, p)) => (h.trim_matches(['[', ']']).to_string(), p.parse::<u16>().unwrap_or(0)),
            None => (netloc.to_string(), 0),
        };
        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        if addrs.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{host} did not resolve")));
        }
        let target = Target { url: url::Url::parse("http://x/").expect("static url"), host: host.to_ascii_lowercase(), port, ip: None };
        let allowed_private = self.explicitly_private(&target);
        for a in &addrs {
            if is_non_public(a.ip()) && !allowed_private {
                return Err(denied(format!("{host} resolves to a private or local address ({}), so the request was blocked", a.ip())));
            }
            if is_loopback_or_unspecified(a.ip()) && AQUA_PORTS.contains(&port) {
                return Err(denied(format!("{host}:{port} resolves to one of Aqua's own services")));
            }
        }
        Ok(addrs)
    }
}

// The address lists live in one place, shared with the browser's site policy (`aqua_mcp_adapters::browser::policy`).
pub use aqua_tool_host::net_addr::is_non_public;
use aqua_tool_host::net_addr::is_loopback_or_unspecified;

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> NetPolicy {
        NetPolicy::new(HttpConfig::default())
    }

    #[test]
    fn scraping_is_on_by_default_and_the_owner_can_switch_it_off() {
        assert!(HttpConfig::default().scrape);
        assert!(serde_json::from_str::<HttpConfig>("{}").unwrap().scrape, "an existing config file keeps it on");
        assert!(!serde_json::from_str::<HttpConfig>(r#"{ "scrape": false }"#).unwrap().scrape);
    }

    fn check(p: &NetPolicy, url: &str) -> Verdict {
        match NetPolicy::parse(url) {
            Ok(t) => p.check(&t),
            Err(e) => Verdict::Deny(e),
        }
    }

    #[test]
    fn non_public_addresses_are_recognised_in_every_spelling() {
        for ip in [
            "127.0.0.1", "127.1.2.3", "10.0.0.1", "172.16.5.5", "172.31.255.255", "192.168.1.1", "169.254.169.254", "100.64.0.1", "100.127.255.255", "0.0.0.0", "0.1.2.3", "224.0.0.1", "255.255.255.255", "240.0.0.1",
            "192.0.0.5", "192.0.2.9", "198.18.0.1", "198.51.100.7", "203.0.113.9", "::1", "::", "fe80::1", "fc00::1", "fd12:3456::1", "ff02::1", "2001:db8::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1", "::ffff:169.254.169.254",
            "64:ff9b::7f00:1", "64:ff9b::a9fe:a9fe", "2002:7f00:1::", "2002:a9fe:a9fe::1",
        ] {
            assert!(is_non_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "172.32.0.1", "100.128.0.1", "2606:4700:4700::1111", "64:ff9b::808:808", "2002:808:808::1", "::ffff:8.8.8.8"] {
            assert!(!is_non_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn credentials_travel_only_over_https_or_to_this_computer() {
        for (url, ok) in [("https://api.github.com/user", true), ("http://api.github.com/user", false), ("http://localhost:3000/", true), ("http://127.0.0.1:5173/", true), ("http://[::1]:5173/", true), ("http://192.168.1.50:8080/", false)] {
            let t = NetPolicy::parse(url).unwrap();
            assert_eq!(NetPolicy::credential_transport_ok(&t), ok, "{url}");
        }
    }

    #[test]
    fn dangerous_urls_are_refused_before_any_connection() {
        let p = policy();
        for url in [
            "file:///C:/Windows/win.ini", "ftp://example.com/x", "javascript:alert(1)", "gopher://example.com/", "not a url", "http://",
            "http://user:pass@example.com/", "http://user@example.com/", "http://169.254.169.254/latest/meta-data/", "http://metadata.google.internal/", "http://127.0.0.1:8080/", "http://localhost:3000/", "http://localhost./", "http://LOCALHOST.:3000/x", "http://metadata.google.internal./",
            "http://10.1.2.3/", "http://[::1]/", "http://[::ffff:7f00:1]/", "http://0x7f.0.0.1/", "http://2130706433/", "http://017700000001/", "http://127.1/", "http://intranet/", "http://printer.local/",
            "http://127.0.0.1:1420/", "http://localhost:8767/api/v1/status", "http://[::1]:8766/", "http://0.0.0.0:8767/",
            "https://example.com:22/", "https://example.com:25/", "http://example.com:6379/", "http://example.com:2375/", "http://example.com:5432/",
        ] {
            assert!(matches!(check(&p, url), Verdict::Deny(_)), "{url} -> {:?}", check(&p, url));
        }
    }

    #[test]
    fn allowed_unknown_and_denied_hosts_are_told_apart() {
        let mut cfg = HttpConfig::default();
        cfg.deny = vec!["*.evil.example".into(), "blocked.com:8443".into()];
        cfg.allow.push("api.github.com".into());
        let p = NetPolicy::new(cfg);
        assert_eq!(check(&p, "https://en.wikipedia.org/wiki/Rust"), Verdict::Allow);
        assert_eq!(check(&p, "https://wikipedia.org/"), Verdict::Allow);
        assert_eq!(check(&p, "https://api.github.com/repos/x/y"), Verdict::Allow);
        assert_eq!(check(&p, "https://some-new-site.org/"), Verdict::Ask);
        assert_eq!(check(&p, "https://notwikipedia.org/"), Verdict::Ask, "a suffix match must respect the dot");
        assert_eq!(check(&p, "https://wikipedia.org.attacker.io/"), Verdict::Ask);
        assert!(matches!(check(&p, "https://a.evil.example/"), Verdict::Deny(_)));
        assert!(matches!(check(&p, "https://blocked.com:8443/"), Verdict::Deny(_)));
        assert_eq!(check(&p, "https://blocked.com/"), Verdict::Ask, "the deny rule names a port");
        let strict = NetPolicy::new(HttpConfig { unknown_hosts: "deny".into(), ..HttpConfig::default() });
        assert!(matches!(check(&strict, "https://some-new-site.org/"), Verdict::Deny(_)));
    }

    #[test]
    fn a_private_host_is_reachable_only_when_named_with_its_port_and_never_on_aquas_ports() {
        let mut cfg = HttpConfig::default();
        cfg.allow_private = vec!["127.0.0.1:5173".into(), "localhost:3000".into(), "127.0.0.1:1420".into(), "192.168.1.50:8080".into()];
        let p = NetPolicy::new(cfg);
        assert_eq!(check(&p, "http://127.0.0.1:5173/"), Verdict::Allow);
        assert_eq!(check(&p, "http://localhost:3000/x"), Verdict::Allow);
        assert_eq!(check(&p, "http://192.168.1.50:8080/"), Verdict::Allow);
        assert!(matches!(check(&p, "http://127.0.0.1:5174/"), Verdict::Deny(_)), "a different port is a different service");
        assert!(matches!(check(&p, "http://192.168.1.51:8080/"), Verdict::Deny(_)));
        assert!(matches!(check(&p, "http://127.0.0.1:1420/"), Verdict::Deny(_)), "Aqua's UI cannot be allowed by config");
    }

    #[test]
    fn hostnames_that_resolve_to_private_addresses_are_refused_by_the_resolver() {
        let p = policy();
        for netloc in ["localhost:80", "127.0.0.1:80", "[::1]:80"] {
            let e = p.resolve(netloc).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{netloc}: {e}");
        }
        let mut cfg = HttpConfig::default();
        cfg.allow_private = vec!["localhost:5173".into(), "127.0.0.1:5173".into()];
        let ok = NetPolicy::new(cfg);
        assert!(!ok.resolve("localhost:5173").unwrap().is_empty());
        assert_eq!(ok.resolve("localhost:5174").unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        let mut aqua = HttpConfig::default();
        aqua.allow_private = vec!["127.0.0.1:8767".into()];
        assert!(NetPolicy::new(aqua).resolve("127.0.0.1:8767").is_err(), "Aqua's gateway stays blocked even when listed");
    }

    #[test]
    fn config_validation_catches_unsafe_auth_and_search_setups() {
        let profile = |hosts: Vec<&str>| AuthProfile { kind: AuthKind::Bearer, secret: "github/token".into(), hosts: hosts.into_iter().map(String::from).collect(), name: None };
        let with = |name: &str, p: AuthProfile| {
            let mut c = HttpConfig::default();
            c.auth.insert(name.into(), p);
            c
        };
        assert!(with("github", profile(vec!["api.github.com"])).validate().is_ok());
        assert!(with("github", profile(vec![])).validate().is_err(), "a credential must name its hosts");
        assert!(with("github", profile(vec!["*"])).validate().is_err());
        assert!(with("bad name", profile(vec!["a.com"])).validate().is_err());
        let mut basic = profile(vec!["a.com"]);
        basic.kind = AuthKind::Basic;
        assert!(with("b", basic).validate().is_err(), "basic needs a user name");
        let mut cfg = HttpConfig::default();
        cfg.search = Some(SearchConfig { engine: SearchEngine::Brave, url: None, auth: Some("missing".into()) });
        assert!(cfg.validate().is_err());
        cfg.search = Some(SearchConfig { engine: SearchEngine::Searxng, url: None, auth: None });
        assert!(cfg.validate().is_err());
        assert!(serde_json::from_str::<HttpConfig>(r#"{"auth": {"g": {"kind": "bearer", "secret": "x", "hosts": ["a.com"], "token": "ghp_real"}}}"#).is_err(), "a token in the config file is rejected");
        assert!(HttpConfig { unknown_hosts: "maybe".into(), ..HttpConfig::default() }.validate().is_err());
    }
}
