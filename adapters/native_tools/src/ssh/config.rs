//! Which remote machines the SSH tools may reach, and how.
//!
//! Hosts are named in `data/tools/ssh.json`. The model passes only an alias: it can never supply
//! a host name, a user, a port or a key path, so it cannot point Aqua at a machine nobody set up.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    /// The name the model uses: letters, digits, `_` and `-`.
    pub alias: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Path to the private key file. Aqua never reads the key; it only hands the path to ssh.
    /// Keys protected by a passphrase are not supported (there is no prompt).
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub description: String,
}

fn default_port() -> u16 {
    22
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConfig {
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
    /// Override the OpenSSH programs (defaults are looked up in `System32\OpenSSH`).
    #[serde(default)]
    pub ssh_path: Option<String>,
    #[serde(default)]
    pub scp_path: Option<String>,
    #[serde(default)]
    pub keyscan_path: Option<String>,
    #[serde(default)]
    pub keygen_path: Option<String>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_s: u64,
}

fn default_connect_timeout() -> u64 {
    10
}

impl Default for SshConfig {
    fn default() -> Self {
        Self { hosts: Vec::new(), ssh_path: None, scp_path: None, keyscan_path: None, keygen_path: None, connect_timeout_s: default_connect_timeout() }
    }
}

fn safe_name(s: &str, extra: &[char]) -> bool {
    !s.is_empty() && s.len() <= 253 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

impl SshConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for h in &self.hosts {
            if h.alias.len() > 40 || !safe_name(&h.alias, &['_', '-']) {
                return Err(format!("alias '{}' must be 1-40 letters, digits, '_' or '-'", h.alias));
            }
            if !seen.insert(h.alias.to_ascii_lowercase()) {
                return Err(format!("alias '{}' is listed twice", h.alias));
            }
            // A host name, an IPv4 address or an IPv6 address: never anything ssh could read as an option.
            let host_ok = safe_name(&h.host, &['.', '-']) || (h.host.contains(':') && h.host.chars().all(|c| c.is_ascii_hexdigit() || c == ':'));
            if !host_ok {
                return Err(format!("host '{}' of '{}' is not a valid host name or address", h.host, h.alias));
            }
            if h.user.len() > 64 || !safe_name(&h.user, &['.', '_', '-']) {
                return Err(format!("user '{}' of '{}' is not a valid user name", h.user, h.alias));
            }
            if h.port == 0 {
                return Err(format!("port of '{}' must be 1-65535", h.alias));
            }
            if let Some(k) = &h.identity_file {
                if k.contains('\0') || k.starts_with('-') || k.contains('\n') {
                    return Err(format!("identity_file of '{}' is not a valid path", h.alias));
                }
            }
        }
        if self.connect_timeout_s == 0 || self.connect_timeout_s > 60 {
            return Err("connect_timeout_s must be 1-60".into());
        }
        Ok(())
    }

    pub fn find(&self, alias: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|h| h.alias.eq_ignore_ascii_case(alias))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str, h: &str, user: &str) -> HostEntry {
        HostEntry { alias: alias.into(), host: h.into(), port: 22, user: user.into(), identity_file: None, description: String::new() }
    }

    fn cfg(hosts: Vec<HostEntry>) -> SshConfig {
        SshConfig { hosts, ..Default::default() }
    }

    #[test]
    fn a_normal_config_is_valid_and_aliases_are_found_case_insensitively() {
        let c = cfg(vec![host("web-1", "203.0.113.5", "deploy"), host("db_2", "db.example.com", "ubuntu"), host("v6", "2001:db8::1", "root")]);
        c.validate().unwrap();
        assert_eq!(c.find("WEB-1").unwrap().host, "203.0.113.5");
        assert!(c.find("nope").is_none());
    }

    #[test]
    fn hosts_that_ssh_could_read_as_options_or_commands_are_rejected() {
        for bad in ["-oProxyCommand=calc", "host name", "a;b", "host$(x)", "", "a/b", "-x"] {
            assert!(cfg(vec![host("a", bad, "u")]).validate().is_err(), "host {bad}");
        }
        for bad in ["-lroot", "a b", "u;x", "", "u@h"] {
            assert!(cfg(vec![host("a", "h.example", bad)]).validate().is_err(), "user {bad}");
        }
        for bad in ["a b", "a/b", "", "-a", &"x".repeat(41)] {
            assert!(cfg(vec![host(bad, "h.example", "u")]).validate().is_err(), "alias {bad}");
        }
    }

    #[test]
    fn duplicates_bad_ports_keys_and_timeouts_are_rejected() {
        assert!(cfg(vec![host("a", "h.example", "u"), host("A", "h2.example", "u")]).validate().is_err());
        let mut h = host("a", "h.example", "u");
        h.port = 0;
        assert!(cfg(vec![h]).validate().is_err());
        let mut h = host("a", "h.example", "u");
        h.identity_file = Some("-oFoo=bar".into());
        assert!(cfg(vec![h]).validate().is_err());
        assert!(SshConfig { connect_timeout_s: 0, ..Default::default() }.validate().is_err());
        assert!(SshConfig::default().validate().is_ok());
        assert!(SshConfig { connect_timeout_s: 61, ..Default::default() }.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_an_error() {
        assert!(serde_json::from_str::<SshConfig>(r#"{"hosts": [], "agent_forwarding": true}"#).is_err());
        assert!(serde_json::from_str::<SshConfig>(r#"{"hosts": [{"alias": "a", "host": "h", "user": "u", "password": "x"}]}"#).is_err());
    }
}
