//! Small helpers for addresses the flow steps are allowed to talk to.

/// The host of an `http://` address, as the URL really addresses it (`127.0.0.1`, `localhost`, `[::1]`), or `None` if it is
/// not a plain `http://` address. It is parsed as a URL: cutting the text at the first `:` or `/` read
/// `http://localhost:80@evil.com/x` as "localhost" while the request would go to evil.com (and carry the token).
pub fn http_host(url: &str) -> Option<String> {
    let u = url::Url::parse(url.trim()).ok()?;
    if u.scheme() != "http" || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    match u.host()? {
        url::Host::Domain(d) => Some(d.trim_end_matches('.').to_ascii_lowercase()),
        url::Host::Ipv4(v4) => Some(v4.to_string()),
        url::Host::Ipv6(v6) => Some(format!("[{v6}]")),
    }
}

/// Is this an `http://` address on this computer?
pub fn is_local_http(url: &str) -> bool {
    matches!(http_host(url).as_deref(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_addresses_that_really_point_at_this_computer_are_local() {
        for ok in ["http://127.0.0.1:5678", "http://localhost:7860/x", "http://LOCALHOST./", "http://[::1]:8080/", "http://127.0.0.1"] {
            assert!(is_local_http(ok), "{ok}");
        }
        for bad in [
            "http://localhost:80@evil.com/x", "http://127.0.0.1@evil.com/", "http://evil.com/?h=localhost", "http://evil.com#127.0.0.1",
            "http://localhost.evil.com/", "https://localhost/", "http://127.0.0.2/", "http://0.0.0.0/", "http://user:pw@localhost/", "localhost:5678", "",
        ] {
            assert!(!is_local_http(bad), "{bad}");
        }
        assert_eq!(http_host("http://[::1]:8080/").as_deref(), Some("[::1]"));
        assert_eq!(http_host("http://localhost:80@evil.com/x"), None, "a user name in the address is never accepted");
    }
}
