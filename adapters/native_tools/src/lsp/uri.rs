//! `file:` addresses, read strictly (card OA12-04, task CU-15 of the OpenCode audit).
//!
//! A language server names a place with an address (`file:///e%3A/proj/src/lib.rs`), and the address is text from a program that reads the project's code: it is not trusted. [`parse_file_uri`]
//! gives back a path only for an address of a plain file on this computer. It refuses, with a reason that says which rule it broke:
//!
//! * a scheme that is not `file`, an address with a host (`file://server/share/x.rs`: a network path), a query or a fragment, a non-ASCII or a control character, a bad `%` escape;
//! * a path with an empty part (`//`: how a network path or a `\\?\` device path is spelled), a `.` or a `..` part (also when it was written `%2e%2e`), a `\`, a NUL;
//! * on Windows: a path that does not start with a drive letter, a part with `:` (an alternate data stream), `*?"<>|`, a part that ends in a space or a dot, a device name (`NUL`, `COM1`).
//!
//! The drive letter is read in either case (`e%3A` and `E:` are the same) and is written in capitals. A path that this accepts is still checked by the file tools' path guard before anything is
//! read: this is the first test, not the only one.

use std::path::{Path, PathBuf};

/// The longest address that is read.
const MAX_URI_CHARS: usize = 2048;

/// Why an address was not taken for a file of this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriError {
    TooLong,
    NotAFileAddress,
    /// An address with a host: a network path.
    HasHost,
    /// A query, a fragment, a control or a non-ASCII character, or a bad `%` escape.
    Malformed,
    /// An empty, `.` or `..` part, a `\`, a NUL.
    BadPart,
    /// On Windows: no drive letter, a device name, a stream, a reserved character.
    NotAPlainWindowsPath,
}

impl std::fmt::Display for UriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UriError::TooLong => "the address is too long",
            UriError::NotAFileAddress => "the address is not a file: address",
            UriError::HasHost => "the address names another computer (a network path)",
            UriError::Malformed => "the address is not well formed",
            UriError::BadPart => "the address has an empty, '.' or '..' part",
            UriError::NotAPlainWindowsPath => "the address is not a plain drive path",
        })
    }
}

/// The path of a `file:` address, for this operating system.
pub fn parse_file_uri(uri: &str) -> Result<PathBuf, UriError> {
    parse_file_uri_for(uri, cfg!(windows))
}

/// [`parse_file_uri`] with the operating system's way of naming a path given (so that both are tested on every system).
pub fn parse_file_uri_for(uri: &str, windows: bool) -> Result<PathBuf, UriError> {
    if uri.len() > MAX_URI_CHARS {
        return Err(UriError::TooLong);
    }
    if !uri.is_ascii() || uri.bytes().any(|b| b < 0x21 || b == 0x7f) {
        return Err(UriError::Malformed);
    }
    let rest = match uri.get(..5) {
        Some(scheme) if scheme.eq_ignore_ascii_case("file:") => &uri[5..],
        _ => return Err(UriError::NotAFileAddress),
    };
    let Some(after_slashes) = rest.strip_prefix("//") else { return Err(UriError::NotAFileAddress) };
    let (authority, path) = match after_slashes.find('/') {
        Some(i) => (&after_slashes[..i], &after_slashes[i..]),
        None => (after_slashes, ""),
    };
    if !(authority.is_empty() || authority.eq_ignore_ascii_case("localhost")) {
        return Err(UriError::HasHost);
    }
    if path.is_empty() {
        return Err(UriError::BadPart);
    }
    if path.contains(['?', '#']) {
        return Err(UriError::Malformed);
    }
    let decoded = percent_decode(path)?;
    if decoded.chars().any(|c| c == '\0' || c == '\\' || c.is_control()) {
        return Err(UriError::BadPart);
    }
    // `decoded` starts with `/`. The parts after it: none of them may be empty (the last one too: a file has a name), `.` or `..`.
    let parts: Vec<&str> = decoded[1..].split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
        return Err(UriError::BadPart);
    }
    if windows {
        let drive = parts[0];
        let mut d = drive.chars();
        let (Some(letter), Some(':'), None) = (d.next(), d.next(), d.next()) else { return Err(UriError::NotAPlainWindowsPath) };
        if !letter.is_ascii_alphabetic() || parts.len() < 2 {
            return Err(UriError::NotAPlainWindowsPath);
        }
        for p in &parts[1..] {
            if !plain_windows_part(p) {
                return Err(UriError::NotAPlainWindowsPath);
            }
        }
        Ok(PathBuf::from(format!("{}:\\{}", letter.to_ascii_uppercase(), parts[1..].join("\\"))))
    } else {
        Ok(PathBuf::from(format!("/{}", parts.join("/"))))
    }
}

/// A part of a Windows path that names an ordinary file or folder.
fn plain_windows_part(p: &str) -> bool {
    if p.chars().any(|c| matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')) || p.ends_with(' ') || p.ends_with('.') {
        return false;
    }
    let stem = p.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    !device
}

fn percent_decode(s: &str) -> Result<String, UriError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3).ok_or(UriError::Malformed)?;
            let h = |c: u8| (c as char).to_digit(16).ok_or(UriError::Malformed);
            out.push((h(hex[0])? * 16 + h(hex[1])?) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| UriError::Malformed)
}

/// The `file:` address of an absolute path, as a language server wants it (`file:///E:/proj/src/lib.rs`): every character that is not a letter, a digit or one of `-._~` is written as `%XX`,
/// except the `/` between the parts and the `:` of a drive.
pub fn file_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/');
    }
    let mut first = true;
    for part in s.split('/') {
        if !first {
            out.push('/');
        }
        // The drive: `E:`.
        if first && part.len() == 2 && part.as_bytes()[1] == b':' && part.as_bytes()[0].is_ascii_alphabetic() {
            out.push_str(part);
        } else {
            for byte in part.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(uri: &str) -> Result<String, UriError> {
        parse_file_uri_for(uri, true).map(|p| p.to_string_lossy().to_string())
    }

    fn posix(uri: &str) -> Result<String, UriError> {
        parse_file_uri_for(uri, false).map(|p| p.to_string_lossy().to_string())
    }

    #[test]
    fn a_plain_address_gives_its_path_and_the_drive_letter_is_read_in_either_case() {
        assert_eq!(win("file:///E:/proj/src/lib.rs").unwrap(), r"E:\proj\src\lib.rs");
        assert_eq!(win("file:///e:/proj/src/lib.rs").unwrap(), r"E:\proj\src\lib.rs");
        // rust-analyzer writes the colon as %3A and the drive in lower case; a space and a non-ASCII letter are escaped.
        assert_eq!(win("file:///e%3A/My%20Proj/caf%C3%A9.rs").unwrap(), "E:\\My Proj\\caf\u{e9}.rs");
        assert_eq!(win("FILE://localhost/c:/a/b.rs").unwrap(), r"C:\a\b.rs");
        assert_eq!(posix("file:///home/me/proj/a.rs").unwrap(), "/home/me/proj/a.rs");
    }

    #[test]
    fn an_address_with_a_host_or_a_network_or_device_path_is_refused() {
        assert_eq!(win("file://server/share/x.rs"), Err(UriError::HasHost));
        assert_eq!(win("file:////server/share/x.rs"), Err(UriError::BadPart), "an empty first part is how a network path is written");
        assert_eq!(win("file:////?/C:/Windows/x.rs"), Err(UriError::Malformed), "the device-path spelling has a '?'");
        assert_eq!(win("file:////./C:/Windows/x.rs"), Err(UriError::BadPart));
        assert_eq!(win("file:///C:/a/NUL"), Err(UriError::NotAPlainWindowsPath));
        assert_eq!(win("file:///C:/a/com1.txt"), Err(UriError::NotAPlainWindowsPath));
        assert_eq!(win("file:///C:/a/b.rs:stream"), Err(UriError::NotAPlainWindowsPath));
        assert_eq!(win("file:///C:/a/b.rs%3Astream"), Err(UriError::NotAPlainWindowsPath));
        assert_eq!(win("file:///C:/a/b .rs "), Err(UriError::Malformed));
        assert_eq!(win("file:///C:/a/b.rs."), Err(UriError::NotAPlainWindowsPath));
        assert_eq!(win("file:///a/b.rs"), Err(UriError::NotAPlainWindowsPath), "no drive letter");
        assert_eq!(win("file:///1:/a/b.rs"), Err(UriError::NotAPlainWindowsPath));
        assert_eq!(win("file:///C:"), Err(UriError::NotAPlainWindowsPath), "a drive is no file");
    }

    #[test]
    fn a_dot_part_an_escaped_one_and_a_backslash_are_refused() {
        for bad in ["file:///C:/a/../Windows/x.rs", "file:///C:/a/./x.rs", "file:///C:/a/%2e%2e/x.rs", "file:///C:/a/%2E%2E/x.rs", "file:///C:/a//x.rs", "file:///C:/a/x%5Cy.rs", "file:///C:/a/x%00.rs", "file:///C:/a/"] {
            assert!(win(bad).is_err(), "{bad} was taken");
            assert!(posix(bad).is_err(), "{bad} was taken");
        }
        assert_eq!(posix("file:///home/../etc/passwd"), Err(UriError::BadPart));
    }

    #[test]
    fn another_scheme_a_query_a_fragment_a_bad_escape_and_a_control_character_are_refused() {
        assert_eq!(win("https://example.com/a.rs"), Err(UriError::NotAFileAddress));
        assert_eq!(win("untitled:Untitled-1"), Err(UriError::NotAFileAddress));
        assert_eq!(win("file:/C:/a.rs"), Err(UriError::NotAFileAddress), "a file address has the two slashes");
        assert_eq!(win("file:///C:/a.rs#L10"), Err(UriError::Malformed));
        assert_eq!(win("file:///C:/a.rs?x=1"), Err(UriError::Malformed));
        assert_eq!(win("file:///C:/a%zz.rs"), Err(UriError::Malformed));
        assert_eq!(win("file:///C:/a%2.rs"), Err(UriError::Malformed));
        assert_eq!(win("file:///C:/a%C3.rs"), Err(UriError::Malformed), "not UTF-8 once decoded");
        assert_eq!(win("file:///C:/a\n.rs"), Err(UriError::Malformed));
        assert_eq!(win("file:///C:/caf\u{e9}.rs"), Err(UriError::Malformed), "a letter that is not ASCII must be escaped");
        assert_eq!(win(&format!("file:///C:/{}", "a".repeat(3000))), Err(UriError::TooLong));
        assert_eq!(win("file://"), Err(UriError::BadPart));
        assert_eq!(win(""), Err(UriError::NotAFileAddress));
    }

    #[test]
    fn an_address_made_for_a_path_reads_back_as_the_same_path() {
        for (path, uri) in [(r"E:\My Proj\src\lib.rs", "file:///E:/My%20Proj/src/lib.rs"), (r"\\?\C:\a\b#c.rs", "file:///C:/a/b%23c.rs"), ("C:\\caf\u{e9}\\x.rs", "file:///C:/caf%C3%A9/x.rs")] {
            assert_eq!(file_uri(Path::new(path)), uri);
            let back = parse_file_uri_for(uri, true).unwrap();
            assert_eq!(back.to_string_lossy().to_lowercase(), path.trim_start_matches(r"\\?\").to_lowercase());
        }
        assert_eq!(file_uri(Path::new("/home/me/a b.rs")), "file:///home/me/a%20b.rs");
    }
}
