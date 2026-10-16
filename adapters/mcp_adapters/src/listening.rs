//! Which addresses a port is listening on, read from the system's own table (`netstat -ano`), and the refusal of a server of ours that opens its port to the
//! whole network.
//!
//! Why: Browser MCP 0.1.3 opens a WebSocket on port 9009 with `new WebSocketServer({ port })`: no address, so every interface, no check of who connects, and the newest
//! connection takes the extension's place. Verified on this machine on 2026-10-07 (it listens on `0.0.0.0:9009` and `[::]:9009` and answered a handshake that said it came
//! from a web page, on the loopback address and on the LAN address). What Aqua sends to the browser goes to whoever connected last.
//!
//! The check only reads the table. It never connects to the port: a handshake to a live Browser MCP would replace the extension's connection.

use anyhow::{bail, Result};
use std::process::Command;
use std::time::{Duration, Instant};

/// Set to `1` by the person who accepts a listener that is open to the network (for example because the firewall blocks the port from other devices).
pub const ALLOW_OPEN_LISTENER_ENV: &str = "AQUA_ALLOW_OPEN_MCP_LISTENER";

/// The refusal, as a type, so a caller can tell it from a server that did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenListener {
    pub label: String,
    pub port: u16,
    pub addresses: Vec<String>,
}

impl std::fmt::Display for OpenListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} listens on port {} on every network interface ({}), so any device that can reach this computer can connect to it, and whoever connects last takes the browser's place. \
Aqua stopped it. Whether the firewall blocks the port from other devices was not checked. If it does, set {ALLOW_OPEN_LISTENER_ENV}=1 to accept this.",
            self.label,
            self.port,
            self.addresses.join(", ")
        )
    }
}

impl std::error::Error for OpenListener {}

/// The local addresses of the TCP sockets that are listening on `port`, from the text of `netstat -ano`.
///
/// A listening socket is the one whose foreign address is all zeros (`0.0.0.0:0` or `[::]:0`): the word "LISTENING" is translated on a Windows that is not in
/// English. The port must match whole (`19009` and `90090` are not `9009`).
pub fn listeners_in(netstat: &str, port: u16) -> Vec<String> {
    let want = port.to_string();
    let mut found: Vec<String> = Vec::new();
    for line in netstat.lines() {
        let mut parts = line.split_whitespace();
        let (Some(proto), Some(local), Some(foreign)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if !proto.eq_ignore_ascii_case("TCP") || !matches!(foreign, "0.0.0.0:0" | "[::]:0" | "*:0") {
            continue;
        }
        let Some((_, local_port)) = local.rsplit_once(':') else { continue };
        if local_port == want && !found.iter().any(|f| f == local) {
            found.push(local.to_string());
        }
    }
    found
}

/// Is this local address (`127.0.0.1:9009`, `[::1]:9009`) one that only this computer can reach?
pub fn is_loopback(address: &str) -> bool {
    let host = address.rsplit_once(':').map_or(address, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    host.starts_with("127.") || host == "::1" || host.starts_with("::ffff:127.")
}

/// The listeners that are not loopback: the ones another device could reach.
pub fn open_to_the_network(listeners: &[String]) -> Vec<String> {
    listeners.iter().filter(|a| !is_loopback(a)).cloned().collect()
}

/// The listeners on `port` now. `None` where the table cannot be read (not Windows, or `netstat` failed): the check then says nothing.
pub fn listeners_now(port: u16) -> Option<Vec<String>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = Command::new("netstat").arg("-ano").creation_flags(CREATE_NO_WINDOW).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(listeners_in(&String::from_utf8_lossy(&out.stdout), port))
    }
    #[cfg(not(windows))]
    {
        let _ = (Command::new("true"), port);
        None
    }
}

/// Refuse a server that has opened `port` to the whole network. Waits up to `wait` for the port to be opened (a server opens it a moment after it starts); a port that
/// never opens, or a table that cannot be read, is not a refusal.
pub fn refuse_when_open_to_the_network(label: &str, port: u16, wait: Duration, allowed: bool) -> Result<()> {
    if allowed {
        return Ok(());
    }
    let deadline = Instant::now() + wait;
    loop {
        match listeners_now(port) {
            None => return Ok(()),
            Some(listeners) if !listeners.is_empty() => {
                let open = open_to_the_network(&listeners);
                if open.is_empty() {
                    return Ok(());
                }
                bail!(OpenListener { label: label.to_string(), port, addresses: open });
            }
            Some(_) if Instant::now() >= deadline => return Ok(()),
            Some(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// The same, with the person's switch read from the environment.
pub fn refuse_when_open(label: &str, port: u16) -> Result<()> {
    let allowed = std::env::var(ALLOW_OPEN_LISTENER_ENV).is_ok_and(|v| v.trim() == "1");
    refuse_when_open_to_the_network(label, port, Duration::from_secs(5), allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1180
  TCP    0.0.0.0:9009           0.0.0.0:0              LISTENING       36740
  TCP    [::]:9009              [::]:0                 LISTENING       36740
  TCP    127.0.0.1:19009        0.0.0.0:0              LISTENING       501
  TCP    127.0.0.1:90090        0.0.0.0:0              LISTENING       502
  TCP    127.0.0.1:54261        127.0.0.1:9009         TIME_WAIT       0
  TCP    192.168.29.61:54263    192.168.29.61:9009     ESTABLISHED     7
  UDP    0.0.0.0:9009           *:*                                    900
";

    #[test]
    fn a_listening_socket_is_found_by_its_foreign_address_and_the_port_must_match_whole() {
        assert_eq!(listeners_in(TABLE, 9009), vec!["0.0.0.0:9009", "[::]:9009"], "not 19009, not 90090, not a connection to it, not a UDP socket");
        assert_eq!(listeners_in(TABLE, 19009), vec!["127.0.0.1:19009"]);
        assert!(listeners_in(TABLE, 9010).is_empty());
        assert!(listeners_in("", 9009).is_empty());
        // The word for the state is translated on a Windows that is not in English: the foreign address is what says it is listening.
        let german = "  TCP    0.0.0.0:9009           0.0.0.0:0              ABHÖREN         36740\n";
        assert_eq!(listeners_in(german, 9009), vec!["0.0.0.0:9009"]);
        // The same socket on a line twice is one listener.
        assert_eq!(listeners_in(&format!("{german}{german}"), 9009).len(), 1);
    }

    #[test]
    fn only_loopback_addresses_are_closed_to_the_network() {
        for closed in ["127.0.0.1:9009", "127.1.2.3:9009", "[::1]:9009", "[::ffff:127.0.0.1]:9009"] {
            assert!(is_loopback(closed), "{closed}");
        }
        for open in ["0.0.0.0:9009", "[::]:9009", "192.168.29.61:9009", "10.0.0.5:9009", "[fe80::1%12]:9009", "[2001:db8::1]:9009", "*:9009"] {
            assert!(!is_loopback(open), "{open}");
        }
        let both = vec!["127.0.0.1:9009".to_string(), "[::]:9009".to_string()];
        assert_eq!(open_to_the_network(&both), vec!["[::]:9009"], "one open address is enough");
        assert!(open_to_the_network(&["[::1]:9009".to_string()]).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_real_listener_on_every_interface_is_refused_one_on_loopback_is_not_and_the_persons_switch_lets_it_through() {
        use std::net::TcpListener;
        let open = TcpListener::bind("0.0.0.0:0").unwrap();
        let open_port = open.local_addr().unwrap().port();
        let shut = TcpListener::bind("127.0.0.1:0").unwrap();
        let shut_port = shut.local_addr().unwrap().port();
        let now = listeners_now(open_port).expect("netstat can be read here");
        assert_eq!(open_to_the_network(&now), vec![format!("0.0.0.0:{open_port}")]);
        let refused = refuse_when_open_to_the_network("Browser MCP", open_port, Duration::from_secs(1), false).unwrap_err();
        let typed = refused.downcast_ref::<OpenListener>().expect("the refusal is a typed error");
        assert_eq!((typed.port, typed.addresses.clone()), (open_port, vec![format!("0.0.0.0:{open_port}")]));
        let said = refused.to_string();
        assert!(said.contains("Browser MCP") && said.contains("every network interface") && said.contains(ALLOW_OPEN_LISTENER_ENV), "{said}");
        refuse_when_open_to_the_network("Browser MCP", shut_port, Duration::from_secs(1), false).expect("loopback is closed to the network");
        refuse_when_open_to_the_network("Browser MCP", open_port, Duration::from_secs(1), true).expect("the person accepted it");
        drop((open, shut));
    }

    #[cfg(windows)]
    #[test]
    fn a_port_nobody_opens_is_not_a_refusal() {
        let free = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() };
        refuse_when_open_to_the_network("Browser MCP", free, Duration::from_millis(300), false).unwrap();
    }
}
