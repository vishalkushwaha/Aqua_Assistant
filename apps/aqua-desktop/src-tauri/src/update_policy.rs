//! The rules an update has to pass before the app offers it. Standard library only: no Tauri, no serde, and the clock is
//! never read here (the time is passed in), so every rule runs in a plain test (`core/tests/update_policy_test.rs` runs
//! this file with the workspace tests).
//!
//! The policy is given facts that were read out of the update information (`latest.json`) as the text that was in the file,
//! the installed version, the highest version this installation has ever accepted, and the time. It answers `Accept` or
//! `Refuse(Reason)`. Every reason has a plain sentence for the page. When in doubt it refuses: a version or a date that
//! cannot be read is a refusal, never an accept.
//!
//! What this does not do: `latest.json` itself is not signed (the signature covers the installer), so `expires` and
//! `min_version` stop a stale, restored or replayed file, not someone who can edit the file. What is signed is the
//! installer and its name; `check_signed_release` ties the announced version to that signed name (see `documentation/UPDATES.md`).

use std::cmp::Ordering;

/// The longest life a manifest may claim: the most `scripts/make_update_manifest.ps1` allows.
pub const MAX_LIFETIME_DAYS: i64 = 90;
const DAY: i64 = 86_400;
/// How far this computer's clock may be behind the manifest's publication date before the clock is not believed.
const CLOCK_SLACK: i64 = DAY;
const STATE_KEY: &str = "highest_version";

// ---------------------------------------------------------------------------------------------------------------------
// Versions

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    // A number is lower than text, as in semantic versioning.
    Num(u64),
    Text(String),
}

/// Numeric dotted parts (one to four) with an optional `-pre` suffix. A pre-release is lower than the release it leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    // Without trailing zeros, so `1.2` and `1.2.0` are the same version.
    parts: Vec<u64>,
    pre: Vec<Ident>,
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 18 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

impl Version {
    /// `None` for anything that is not plainly a version: spaces, a leading `v`, build metadata (`+`), empty parts.
    pub fn parse(text: &str) -> Option<Version> {
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = Vec::new();
        for piece in core.split('.') {
            parts.push(number(piece)?);
        }
        if parts.len() > 4 {
            return None;
        }
        while parts.last() == Some(&0) {
            parts.pop();
        }
        let mut idents = Vec::new();
        if let Some(pre) = pre {
            for id in pre.split('.') {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return None;
                }
                if id.bytes().all(|b| b.is_ascii_digit()) {
                    idents.push(Ident::Num(number(id)?));
                } else {
                    idents.push(Ident::Text(id.to_string()));
                }
            }
        }
        Some(Version { parts, pre: idents })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Time

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to the given day of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DDTHH:MM:SSZ` (UTC, exactly that shape) to seconds since 1970. `None` for any other text or a day that does
/// not exist (30 February, 29 February of a year that is not a leap year, hour 24).
pub fn parse_utc(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 20 {
        return None;
    }
    for (i, c) in b.iter().enumerate() {
        let fits = match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        };
        if !fits {
            return None;
        }
    }
    // Every byte is now plain ASCII, so these slices are on character boundaries.
    let field = |from: usize, to: usize| -> i64 { text[from..to].parse().unwrap_or(-1) };
    let (year, month, day) = (field(0, 4), field(5, 7), field(8, 10));
    let (hour, minute, second) = (field(11, 13), field(14, 16), field(17, 19));
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) {
        return None;
    }
    Some(days_from_civil(year, month, day) * DAY + hour * 3600 + minute * 60 + second)
}

// ---------------------------------------------------------------------------------------------------------------------
// The decision

/// What the update information says, as the text that was in the file.
#[derive(Debug, Clone, Copy)]
pub struct Manifest<'a> {
    pub version: &'a str,
    pub expires: Option<&'a str>,
    pub min_version: Option<&'a str>,
    pub pub_date: Option<&'a str>,
}

/// What is known on this computer.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub installed: &'a str,
    /// The highest version this installation has ever accepted; `None` before the first accepted update.
    pub highest_accepted: Option<&'a str>,
    /// The update address is on this computer (a release being tried out), not on another one.
    pub endpoint_is_local: bool,
    /// Seconds since 1970. The caller reads the clock.
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Refuse(Reason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A field that is there could not be read.
    Unreadable { what: &'static str, text: String },
    /// No `expires`, and the update address is not this computer.
    ExpiryMissing,
    Expired { expires: String },
    ExpiryBeforePublication,
    LifetimeTooLong,
    ClockBehind { pub_date: String },
    /// Lower than the highest version this installation has accepted: an old file put back.
    Rollback { manifest: String, highest: String },
    /// Equal to or lower than the installed version. Not a warning: there is simply nothing to install.
    NotNewer { manifest: String, installed: String },
    /// The installed version is below `min_version`: the person installs the newest version by hand.
    MustInstallByHand { installed: String, minimum: String },
    /// `update_state.json` cannot be read.
    StateDamaged,
    /// The installer was signed as one version and the update information announces another.
    SignedVersionMismatch { signed: String, announced: String },
    /// The signature says nothing the announced version can be checked against.
    SignedVersionMissing,
}

/// Text from a file or a server, made safe to put in a sentence: no control characters, at most 40 characters.
fn clip(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).take(40).collect();
    if text.chars().count() > 40 {
        clean + "..."
    } else {
        clean
    }
}

fn unreadable(what: &'static str, text: &str) -> Reason {
    Reason::Unreadable { what, text: clip(text) }
}

impl Reason {
    /// False only for "nothing newer": the page should not warn about that.
    pub fn is_warning(&self) -> bool {
        !matches!(self, Reason::NotNewer { .. })
    }

    /// A plain sentence the page can show.
    pub fn sentence(&self) -> String {
        match self {
            Reason::Unreadable { what, text } => format!("Aqua could not read {what} (\"{text}\"), so it did not use the update information."),
            Reason::ExpiryMissing => "The update information has no expiry date. An update address on another computer has to give one, so Aqua did not use it. The publisher has to publish it again.".into(),
            Reason::Expired { expires } => format!("The update information expired on {expires}, so Aqua did not use it, and cannot tell whether a newer version exists. The publisher has to publish fresh information."),
            Reason::ExpiryBeforePublication => "The update information expires before the day it was published, so Aqua did not use it.".into(),
            Reason::LifetimeTooLong => format!("The update information claims to stay valid for more than {MAX_LIFETIME_DAYS} days, which Aqua's own releases never do, so Aqua did not use it."),
            Reason::ClockBehind { pub_date } => format!("This computer's clock is more than a day behind the day the update information was published ({pub_date}), so Aqua cannot tell whether it is still fresh. Check the date and time of this computer."),
            Reason::Rollback { manifest, highest } => format!("The update address offers version {manifest}, but this installation has already accepted version {highest}. Aqua never goes back to an older version. This can mean an old file was put back on the update address, or that it was tampered with."),
            Reason::NotNewer { manifest, installed } => {
                if manifest == installed {
                    format!("Aqua is up to date (version {installed}).")
                } else {
                    format!("The update address offers version {manifest}, which is not newer than the installed version {installed}, so there is nothing to install.")
                }
            }
            Reason::MustInstallByHand { installed, minimum } => format!("Version {installed} is too old to update by itself: this update needs at least version {minimum}. Download the installer of the newest version and run it by hand."),
            Reason::StateDamaged => "The file update_state.json in the data folder, where Aqua keeps the highest version it has accepted, cannot be read, so updates are paused. Delete that file to start the record again (Aqua then trusts the next update it is offered).".into(),
            Reason::SignedVersionMismatch { signed, announced } => format!("The installer was signed as \"{signed}\" but the update information announces version {announced}, so it was not installed. This can mean the update address was tampered with."),
            Reason::SignedVersionMissing => "The signature of the installer does not say which version it is, so Aqua cannot check it against the update information and did not install it.".into(),
        }
    }
}

fn parse_field(what: &'static str, text: &str) -> Result<Version, Reason> {
    Version::parse(text).ok_or_else(|| unreadable(what, text))
}

/// May this update be offered? The order of the checks: every field that is there must be readable; the update
/// information must be fresh; it must not be older than what this installation has accepted; it must be newer than what
/// is installed; and the installed version must be recent enough to update by itself.
pub fn decide(manifest: &Manifest, context: &Context) -> Verdict {
    match judge(manifest, context) {
        Ok(()) => Verdict::Accept,
        Err(reason) => Verdict::Refuse(reason),
    }
}

fn judge(m: &Manifest, c: &Context) -> Result<(), Reason> {
    let announced = parse_field("the version in the update information", m.version)?;
    let installed = parse_field("the installed version", c.installed)?;
    let highest = c.highest_accepted.map(|t| parse_field("the highest version accepted before", t)).transpose()?;
    let minimum = m.min_version.map(|t| parse_field("the minimum version in the update information", t)).transpose()?;

    // Fresh? A local address (a release being tried out) may leave `expires` out; one that is there must be readable.
    match m.expires {
        None if c.endpoint_is_local => {}
        None => return Err(Reason::ExpiryMissing),
        Some(text) => {
            let expires = parse_utc(text).ok_or_else(|| unreadable("the expiry date in the update information", text))?;
            if let Some(published_text) = m.pub_date {
                let published = parse_utc(published_text).ok_or_else(|| unreadable("the publication date in the update information", published_text))?;
                if expires <= published {
                    return Err(Reason::ExpiryBeforePublication);
                }
                if expires - published > MAX_LIFETIME_DAYS * DAY {
                    return Err(Reason::LifetimeTooLong);
                }
                if c.now < published - CLOCK_SLACK {
                    return Err(Reason::ClockBehind { pub_date: clip(published_text) });
                }
            }
            // Valid until the expiry second, not including it.
            if c.now >= expires {
                return Err(Reason::Expired { expires: clip(text) });
            }
        }
    }

    if let Some(highest) = &highest {
        if announced < *highest {
            return Err(Reason::Rollback { manifest: clip(m.version), highest: clip(c.highest_accepted.unwrap_or("")) });
        }
    }
    if announced <= installed {
        return Err(Reason::NotNewer { manifest: clip(m.version), installed: clip(c.installed) });
    }
    if let Some(minimum) = &minimum {
        if installed < *minimum {
            return Err(Reason::MustInstallByHand { installed: clip(c.installed), minimum: clip(m.min_version.unwrap_or("")) });
        }
    }
    Ok(())
}

/// Plain `http` is accepted only for an address on this computer; this says which hosts those are.
pub fn host_is_this_computer(host: Option<&str>) -> bool {
    matches!(host, Some("127.0.0.1" | "localhost" | "[::1]"))
}

// ---------------------------------------------------------------------------------------------------------------------
// The signed installer

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let b = text.as_bytes();
    if b.is_empty() || b.len() % 4 != 0 {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let chunks = b.len() / 4;
    let mut out = Vec::with_capacity(chunks * 3);
    for (i, chunk) in b.chunks(4).enumerate() {
        let pad = if chunk[3] != b'=' { 0 } else if chunk[2] != b'=' { 1 } else { 2 };
        if pad > 0 && i + 1 != chunks {
            return None;
        }
        let mut acc = 0u32;
        for (j, c) in chunk.iter().enumerate() {
            let digit = if j >= 4 - pad {
                if *c != b'=' {
                    return None;
                }
                0
            } else {
                value(*c)?
            };
            acc = (acc << 6) | digit;
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// The trusted comment of a signature as the update information carries it (the base64 of the signature file, whose third
/// line is `trusted comment: ...`). The comment is covered by the signature, but only once the updater has checked the
/// signature: read it after the download, never before.
pub fn trusted_comment(signature_base64: &str) -> Option<String> {
    let bytes = base64_decode(signature_base64.trim())?;
    let text = String::from_utf8(bytes).ok()?;
    text.lines().nth(2)?.strip_prefix("trusted comment: ").map(str::to_string)
}

/// The announced version must be the version the installer was signed as. The Tauri tool signs the installer's file name
/// (`<product>_<version>_<arch>-setup.exe`) into the comment, and newer ones a `version:` field too; either is accepted, and
/// a comment with neither is refused. Without this, someone who can edit the update information could pair a high version
/// number with an old installer that still carries a valid signature.
pub fn check_signed_release(comment: Option<&str>, announced: &str) -> Result<(), Reason> {
    let announced_version = parse_field("the version in the update information", announced)?;
    let Some(comment) = comment else { return Err(Reason::SignedVersionMissing) };
    let field = |key: &str| comment.split('\t').find_map(|f| f.strip_prefix(key));
    if let Some(signed) = field("version:") {
        let signed_version = parse_field("the version the installer was signed as", signed.trim_start_matches('v'))?;
        return if signed_version == announced_version {
            Ok(())
        } else {
            Err(Reason::SignedVersionMismatch { signed: clip(signed), announced: clip(announced) })
        };
    }
    if let Some(name) = field("file:") {
        return if name.contains(&format!("_{announced}_")) {
            Ok(())
        } else {
            Err(Reason::SignedVersionMismatch { signed: clip(name), announced: clip(announced) })
        };
    }
    Err(Reason::SignedVersionMissing)
}

// ---------------------------------------------------------------------------------------------------------------------
// The saved state: `<data folder>\update_state.json`, `{"highest_version": "0.3.0"}`

/// The text to save once a verified update is about to be installed. `None` if the version is not a readable version.
pub fn state_text(highest: &str) -> Option<String> {
    Version::parse(highest)?;
    Some(format!("{{\"{STATE_KEY}\": \"{highest}\"}}\n"))
}

/// The version in a saved state. A file that is there and cannot be read is `StateDamaged` (updates pause), not "no state".
pub fn read_state(text: &str) -> Result<String, Reason> {
    let after_key = text.split_once(&format!("\"{STATE_KEY}\"")).ok_or(Reason::StateDamaged)?.1;
    let rest = after_key.trim_start().strip_prefix(':').ok_or(Reason::StateDamaged)?;
    let rest = rest.trim_start().strip_prefix('"').ok_or(Reason::StateDamaged)?;
    let value = rest.split_once('"').ok_or(Reason::StateDamaged)?.0;
    Version::parse(value).ok_or(Reason::StateDamaged)?;
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Seconds since 1970 for the dates used below, worked out with Python's calendar.timegm, not with this code.
    const T_2000: i64 = 946_684_800; // 2000-01-01T00:00:00Z
    const T_2024_LEAP_END: i64 = 1_709_251_199; // 2024-02-29T23:59:59Z
    const T_2024_MARCH: i64 = 1_709_251_200; // 2024-03-01T00:00:00Z
    const T_NOW: i64 = 1_791_417_600; // 2026-10-08T00:00:00Z
    const T_YEAR_END: i64 = 1_798_761_599; // 2026-12-31T23:59:59Z
    const T_NEW_YEAR: i64 = 1_798_761_600; // 2027-01-01T00:00:00Z

    const PUB: &str = "2026-10-08T00:00:00Z";
    const EXPIRES: &str = "2026-11-07T00:00:00Z"; // 30 days later

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} should be a version"))
    }

    fn manifest<'a>(version: &'a str, expires: Option<&'a str>) -> Manifest<'a> {
        Manifest { version, expires, min_version: None, pub_date: Some(PUB) }
    }

    fn context<'a>(installed: &'a str, highest: Option<&'a str>) -> Context<'a> {
        Context { installed, highest_accepted: highest, endpoint_is_local: false, now: T_NOW + 3600 }
    }

    fn refused(manifest: &Manifest, context: &Context) -> Reason {
        match decide(manifest, context) {
            Verdict::Refuse(reason) => reason,
            Verdict::Accept => panic!("expected a refusal, got Accept"),
        }
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.1") > v("0.1.0"));
        assert!(v("2.0") > v("1.99.99"));
        assert_eq!(v("1.2"), v("1.2.0"));
        assert_eq!(v("1.2"), v("1.2.0.0"));
        assert!(v("1.2.0.1") > v("1.2"));
        assert!(v("1.2.0.1") < v("1.2.1"));
        assert_eq!(v("0.0.0"), v("0"));
        assert!(v("0.0.1") > v("0.0.0"));
    }

    #[test]
    fn a_pre_release_is_lower_than_its_release_and_the_order_follows_semantic_versioning() {
        let ordered = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} should be lower than {}", pair[0], pair[1]);
            assert!(v(pair[1]) > v(pair[0]));
        }
        assert!(v("1.0.0-rc.1") < v("1.0.1-alpha"));
        assert!(v("0.2.0-rc.1") > v("0.1.9"));
        assert_eq!(v("1.0.0-beta-1"), v("1.0.0-beta-1"));
        assert!(v("1.0.0-beta-1") != v("1.0.0-beta-2"));
    }

    #[test]
    fn what_is_not_plainly_a_version_is_unreadable() {
        for bad in [
            "", " ", " 1.0.0", "1.0.0 ", "1..0", "1.", ".1", "1.x", "v1.0.0", "1.0.0-", "1.0.0-a..b", "1.0.0-.a", "1.0.0+5", "01.0.0", "1.00.0", "1.2.3.4.5", "-1.0.0",
            "99999999999999999999.0.0", "1.0.0-01", "1.0.0-a_b", "latest", "1,0,0", "1.0.0\n",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad:?} must not be a version");
        }
        for good in ["0", "1", "0.1.0", "10.20.30.40", "1.0.0-rc.1", "0.0.0"] {
            assert!(Version::parse(good).is_some(), "{good:?} must be a version");
        }
    }

    #[test]
    fn dates_are_read_to_the_second_and_the_known_values_come_out() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("1970-01-01T00:00:01Z"), Some(1));
        assert_eq!(parse_utc("2000-01-01T00:00:00Z"), Some(T_2000));
        assert_eq!(parse_utc("2026-10-08T00:00:00Z"), Some(T_NOW));
        assert_eq!(parse_utc("2026-10-08T12:30:45Z"), Some(1_791_462_645));
        assert_eq!(parse_utc("9999-12-31T23:59:59Z"), Some(253_402_300_799));
    }

    #[test]
    fn leap_days_and_month_ends() {
        // 2000 is a leap year (divisible by 400), 2024 is, 2100 is not (divisible by 100), 2026 is not.
        assert_eq!(parse_utc("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_utc("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_utc("2024-02-29T23:59:59Z"), Some(T_2024_LEAP_END));
        assert_eq!(parse_utc("2024-03-01T00:00:00Z"), Some(T_2024_MARCH));
        assert_eq!(T_2024_MARCH, T_2024_LEAP_END + 1);
        assert_eq!(parse_utc("2028-02-29T12:00:00Z"), Some(1_835_438_400));
        assert_eq!(parse_utc("2100-02-28T00:00:00Z"), Some(4_107_456_000));
        assert_eq!(parse_utc("2100-03-01T00:00:00Z"), Some(4_107_542_400));
        assert_eq!(parse_utc("2100-02-29T00:00:00Z"), None);
        assert_eq!(parse_utc("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_utc("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_utc("2026-02-28T00:00:00Z").map(|s| s + DAY), parse_utc("2026-03-01T00:00:00Z"));
        // Month ends: 31-day months accept the 31st, 30-day months do not.
        for month in ["01", "03", "05", "07", "08", "10", "12"] {
            assert!(parse_utc(&format!("2026-{month}-31T00:00:00Z")).is_some(), "{month}-31");
        }
        for month in ["04", "06", "09", "11"] {
            assert!(parse_utc(&format!("2026-{month}-30T00:00:00Z")).is_some(), "{month}-30");
            assert_eq!(parse_utc(&format!("2026-{month}-31T00:00:00Z")), None, "{month}-31");
        }
        // The last second of a year and the first of the next differ by one second.
        assert_eq!(parse_utc("2026-12-31T23:59:59Z"), Some(T_YEAR_END));
        assert_eq!(parse_utc("2027-01-01T00:00:00Z"), Some(T_NEW_YEAR));
        assert_eq!(T_NEW_YEAR, T_YEAR_END + 1);
    }

    #[test]
    fn dates_that_are_not_exactly_the_shape_are_unreadable() {
        for bad in [
            "",
            "2026-10-08",
            "2026-10-08 00:00:00Z",
            "2026-10-08T00:00:00",
            "2026-10-08T00:00:00z",
            "2026-10-08T00:00:00+00:00",
            "2026-10-08T00:00:00.000Z",
            "2026-10-08T00:00:00Z ",
            " 2026-10-08T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-00-10T00:00:00Z",
            "2026-10-00T00:00:00Z",
            "2026-10-32T00:00:00Z",
            "2026-10-08T24:00:00Z",
            "2026-10-08T00:60:00Z",
            "2026-10-08T00:00:60Z",
            "1969-12-31T23:59:59Z",
            "0000-01-01T00:00:00Z",
            "2026-1-8T0:0:0Z",
            "+026-10-08T00:00:00Z",
            "2026-10-08T00:00:0\u{00e9}",
            "\u{ff12}\u{ff10}26-10-08T00:00:00Z",
            "tomorrow",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_fresh_newer_update_is_accepted_and_so_is_the_first_run_with_no_saved_state() {
        let m = manifest("0.2.0", Some(EXPIRES));
        assert_eq!(decide(&m, &context("0.1.0", None)), Verdict::Accept);
        assert_eq!(decide(&m, &context("0.1.0", Some("0.1.0"))), Verdict::Accept);
        assert_eq!(decide(&m, &context("0.1.0", Some("0.2.0"))), Verdict::Accept);
    }

    #[test]
    fn an_expired_manifest_is_refused_and_one_expiring_exactly_now_is_expired_too() {
        let m = manifest("0.2.0", Some(EXPIRES));
        let expires = parse_utc(EXPIRES).unwrap();
        let mut c = context("0.1.0", None);
        c.now = expires - 1;
        assert_eq!(decide(&m, &c), Verdict::Accept);
        c.now = expires;
        assert_eq!(refused(&m, &c), Reason::Expired { expires: EXPIRES.into() });
        c.now = expires + 1;
        assert!(matches!(refused(&m, &c), Reason::Expired { .. }));
        // An expired manifest is a warning even when it offers nothing new: it may be hiding a newer version.
        let same = manifest("0.1.0", Some(EXPIRES));
        assert!(matches!(refused(&same, &c), Reason::Expired { .. }));
        assert!(refused(&same, &c).is_warning());
    }

    #[test]
    fn a_missing_expiry_fails_closed_for_another_computer_and_is_allowed_for_this_one() {
        let m = Manifest { version: "0.2.0", expires: None, min_version: None, pub_date: Some(PUB) };
        let remote = context("0.1.0", None);
        assert_eq!(refused(&m, &remote), Reason::ExpiryMissing);
        assert!(refused(&m, &remote).sentence().contains("another computer"));
        let mut local = context("0.1.0", None);
        local.endpoint_is_local = true;
        assert_eq!(decide(&m, &local), Verdict::Accept);
        // A local address is allowed to leave the field out, not to fill it with something unreadable or already past.
        let garbled = Manifest { expires: Some("soon"), ..m };
        assert!(matches!(refused(&garbled, &local), Reason::Unreadable { .. }));
        let past = Manifest { expires: Some("2026-10-09T00:00:00Z"), ..m };
        local.now = parse_utc("2026-10-09T00:00:00Z").unwrap();
        assert!(matches!(refused(&past, &local), Reason::Expired { .. }));
    }

    #[test]
    fn an_unreadable_expiry_is_refused_for_both() {
        for expires in ["soon", "2026-11-07", "2026-11-07T00:00:00+00:00", "", "2026-02-30T00:00:00Z", "1789000000"] {
            let m = manifest("0.2.0", Some(expires));
            let mut c = context("0.1.0", None);
            assert!(matches!(refused(&m, &c), Reason::Unreadable { .. }), "{expires:?}");
            c.endpoint_is_local = true;
            assert!(matches!(refused(&m, &c), Reason::Unreadable { .. }), "{expires:?} (local)");
        }
    }

    #[test]
    fn an_unreadable_version_is_refused_wherever_it_is() {
        let good = manifest("0.2.0", Some(EXPIRES));
        for bad in ["", "latest", "v0.2.0", "0.2.0 beta", "0.2.0+7"] {
            let m = manifest(bad, Some(EXPIRES));
            assert!(matches!(decide(&m, &context("0.1.0", None)), Verdict::Refuse(Reason::Unreadable { .. })), "{bad:?}");
        }
        assert!(matches!(refused(&good, &context("dev", None)), Reason::Unreadable { what: "the installed version", .. }));
        assert!(matches!(refused(&good, &context("0.1.0", Some("garbage"))), Reason::Unreadable { .. }));
        let bad_min = Manifest { min_version: Some("old"), ..good };
        assert!(matches!(refused(&bad_min, &context("0.1.0", None)), Reason::Unreadable { .. }));
        let bad_pub = Manifest { pub_date: Some("yesterday"), ..good };
        assert!(matches!(refused(&bad_pub, &context("0.1.0", None)), Reason::Unreadable { .. }));
        // A readable version never turns an unreadable one into an accept.
        let both = Manifest { min_version: Some("old"), ..manifest("9.9.9", Some(EXPIRES)) };
        assert!(matches!(refused(&both, &context("0.1.0", None)), Reason::Unreadable { .. }));
    }

    #[test]
    fn a_manifest_lower_than_the_highest_accepted_is_a_rollback() {
        let m = manifest("0.2.5", Some(EXPIRES));
        let reason = refused(&m, &context("0.2.0", Some("0.3.0")));
        assert_eq!(reason, Reason::Rollback { manifest: "0.2.5".into(), highest: "0.3.0".into() });
        assert!(reason.is_warning());
        assert!(reason.sentence().contains("0.3.0") && reason.sentence().contains("0.2.5"));
        // Equal to the highest accepted is not a rollback (the same version offered again after a failed install).
        assert_eq!(decide(&manifest("0.3.0", Some(EXPIRES)), &context("0.2.0", Some("0.3.0"))), Verdict::Accept);
        // A pre-release of the highest is lower than it.
        assert!(matches!(refused(&manifest("0.3.0-rc.1", Some(EXPIRES)), &context("0.2.0", Some("0.3.0"))), Reason::Rollback { .. }));
        // The rollback is named even when the installed version is also higher than the manifest.
        assert!(matches!(refused(&manifest("0.2.5", Some(EXPIRES)), &context("0.3.0", Some("0.3.0"))), Reason::Rollback { .. }));
    }

    #[test]
    fn a_manifest_that_is_not_newer_than_the_installed_version_is_not_an_update_and_not_a_warning() {
        let equal = refused(&manifest("0.2.0", Some(EXPIRES)), &context("0.2.0", None));
        assert_eq!(equal, Reason::NotNewer { manifest: "0.2.0".into(), installed: "0.2.0".into() });
        assert!(!equal.is_warning());
        assert!(equal.sentence().contains("up to date"));
        let lower = refused(&manifest("0.1.0", Some(EXPIRES)), &context("0.2.0", None));
        assert!(!lower.is_warning());
        assert!(lower.sentence().contains("not newer"));
        // A pre-release of the installed version is not newer; the release after an installed pre-release is.
        assert!(matches!(refused(&manifest("0.2.0-rc.1", Some(EXPIRES)), &context("0.2.0", None)), Reason::NotNewer { .. }));
        assert_eq!(decide(&manifest("0.2.0", Some(EXPIRES)), &context("0.2.0-rc.1", None)), Verdict::Accept);
    }

    #[test]
    fn an_installed_version_below_min_version_must_install_by_hand_and_the_person_is_told() {
        let m = Manifest { min_version: Some("0.2.0"), ..manifest("0.3.0", Some(EXPIRES)) };
        let reason = refused(&m, &context("0.1.0", None));
        assert_eq!(reason, Reason::MustInstallByHand { installed: "0.1.0".into(), minimum: "0.2.0".into() });
        assert!(reason.is_warning());
        assert!(reason.sentence().contains("by hand"));
        // Equal to min_version is enough; above it is enough; a pre-release of it is not.
        assert_eq!(decide(&m, &context("0.2.0", None)), Verdict::Accept);
        assert_eq!(decide(&m, &context("0.2.5", None)), Verdict::Accept);
        assert!(matches!(refused(&m, &context("0.2.0-rc.1", None)), Reason::MustInstallByHand { .. }));
        // With no min_version the field is simply not there.
        assert_eq!(decide(&manifest("0.3.0", Some(EXPIRES)), &context("0.0.1", None)), Verdict::Accept);
    }

    #[test]
    fn the_publication_date_catches_a_nonsense_life_and_a_clock_that_is_wrong() {
        let mut c = context("0.1.0", None);
        // Expires on the publication second or before it.
        assert_eq!(refused(&manifest("0.2.0", Some(PUB)), &c), Reason::ExpiryBeforePublication);
        assert_eq!(refused(&manifest("0.2.0", Some("2026-10-07T00:00:00Z")), &c), Reason::ExpiryBeforePublication);
        // 90 days is the longest; 90 days and one second is too long.
        let day90 = "2027-01-06T00:00:00Z";
        assert_eq!(parse_utc(day90).unwrap() - parse_utc(PUB).unwrap(), 90 * DAY);
        assert_eq!(decide(&manifest("0.2.0", Some(day90)), &c), Verdict::Accept);
        assert_eq!(refused(&manifest("0.2.0", Some("2027-01-06T00:00:01Z")), &c), Reason::LifetimeTooLong);
        // A clock more than a day behind the publication date is not believed; exactly a day behind is.
        c.now = T_NOW - DAY;
        assert_eq!(decide(&manifest("0.2.0", Some(EXPIRES)), &c), Verdict::Accept);
        c.now = T_NOW - DAY - 1;
        assert_eq!(refused(&manifest("0.2.0", Some(EXPIRES)), &c), Reason::ClockBehind { pub_date: PUB.into() });
        // A clock stuck at 1970 would otherwise make every old manifest look fresh.
        c.now = 0;
        assert!(matches!(refused(&manifest("0.2.0", Some(EXPIRES)), &c), Reason::ClockBehind { .. }));
        // Without a publication date those two checks are skipped, the expiry is still enforced.
        let no_pub = Manifest { pub_date: None, ..manifest("0.2.0", Some(EXPIRES)) };
        c.now = T_NOW;
        assert_eq!(decide(&no_pub, &c), Verdict::Accept);
        c.now = parse_utc(EXPIRES).unwrap();
        assert!(matches!(refused(&no_pub, &c), Reason::Expired { .. }));
    }

    #[test]
    fn every_reason_has_a_sentence_that_says_something_and_text_from_the_file_cannot_smuggle_controls() {
        let long = "x".repeat(200);
        let reasons = vec![
            Reason::Unreadable { what: "the installed version", text: "dev".into() },
            Reason::ExpiryMissing,
            Reason::Expired { expires: EXPIRES.into() },
            Reason::ExpiryBeforePublication,
            Reason::LifetimeTooLong,
            Reason::ClockBehind { pub_date: PUB.into() },
            Reason::Rollback { manifest: "0.2.5".into(), highest: "0.3.0".into() },
            Reason::NotNewer { manifest: "0.2.0".into(), installed: "0.2.0".into() },
            Reason::MustInstallByHand { installed: "0.1.0".into(), minimum: "0.2.0".into() },
            Reason::StateDamaged,
            Reason::SignedVersionMismatch { signed: "a".into(), announced: "0.2.0".into() },
            Reason::SignedVersionMissing,
        ];
        for r in &reasons {
            let s = r.sentence();
            assert!(s.len() > 20 && s.ends_with('.'), "{s}");
            assert!(!s.contains('\n'));
        }
        assert!(Reason::Expired { expires: EXPIRES.into() }.sentence().contains(EXPIRES));
        assert_eq!(reasons.iter().filter(|r| !r.is_warning()).count(), 1);
        // Hostile text in a field is clipped and stripped before it reaches a sentence.
        let m = Manifest { version: "0.2.0\u{1b}[31m\nred", expires: Some(EXPIRES), min_version: None, pub_date: None };
        let s = refused(&m, &context("0.1.0", None)).sentence();
        assert!(!s.contains('\u{1b}') && !s.contains("\n"), "{s:?}");
        let m = Manifest { version: &long, expires: Some(EXPIRES), min_version: None, pub_date: None };
        let s = refused(&m, &context("0.1.0", None)).sentence();
        assert!(s.len() < 250 && s.contains("..."), "{s}");
    }

    #[test]
    fn only_this_computer_counts_as_local() {
        for local in ["127.0.0.1", "localhost", "[::1]"] {
            assert!(host_is_this_computer(Some(local)), "{local}");
        }
        for other in ["127.0.0.1.evil.com", "localhost.evil.com", "example.com", "0.0.0.0", "127.0.0.2", ""] {
            assert!(!host_is_this_computer(Some(other)), "{other}");
        }
        assert!(!host_is_this_computer(None));
    }

    #[test]
    fn base64_is_decoded_strictly() {
        assert_eq!(base64_decode("YQ=="), Some(b"a".to_vec()));
        assert_eq!(base64_decode("YWI="), Some(b"ab".to_vec()));
        assert_eq!(base64_decode("YWJj"), Some(b"abc".to_vec()));
        assert_eq!(base64_decode("YWJjZA=="), Some(b"abcd".to_vec()));
        assert_eq!(base64_decode("+/+/"), Some(vec![0xfb, 0xff, 0xbf]));
        for bad in ["", "YQ=", "YQ", "Y=Q=", "YQ==YWJj", "Y!==", "YQ=a", "=YWI", "YWJj\n"] {
            assert_eq!(base64_decode(bad), None, "{bad:?}");
        }
    }

    // The signature file of a Tauri build as the update information carries it: the base64 of four lines (untrusted
    // comment, signature, trusted comment, global signature). The signature bytes here are filler: only the comment is read.
    // Trusted comment: "timestamp:1791417600\tfile:Aqua Assistant_0.3.0_x64-setup.exe"
    const SIG_FILE_NAME: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE9PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxNDE3NjAwCWZpbGU6QXF1YSBBc3Npc3RhbnRfMC4zLjBfeDY0LXNldHVwLmV4ZQpRVUZCUVE9PQo=";
    // The same with "\tversion:0.3.0" added, as newer Tauri tools write it.
    const SIG_WITH_VERSION: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE9PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxNDE3NjAwCWZpbGU6QXF1YSBBc3Npc3RhbnRfMC4zLjBfeDY0LXNldHVwLmV4ZQl2ZXJzaW9uOjAuMy4wClFVRkJRUT09Cg==";

    #[test]
    fn the_trusted_comment_is_read_out_of_the_signature_file() {
        assert_eq!(trusted_comment(SIG_FILE_NAME).as_deref(), Some("timestamp:1791417600\tfile:Aqua Assistant_0.3.0_x64-setup.exe"));
        assert_eq!(trusted_comment(&format!("  {SIG_FILE_NAME}\n")).as_deref(), Some("timestamp:1791417600\tfile:Aqua Assistant_0.3.0_x64-setup.exe"));
        assert_eq!(trusted_comment(SIG_WITH_VERSION).as_deref(), Some("timestamp:1791417600\tfile:Aqua Assistant_0.3.0_x64-setup.exe\tversion:0.3.0"));
        // Not base64, not text, too few lines, or a third line that is not a trusted comment.
        assert_eq!(trusted_comment(""), None);
        assert_eq!(trusted_comment("not base64!"), None);
        assert_eq!(trusted_comment("//79"), None); // bytes ff fe fd: not UTF-8
        assert_eq!(trusted_comment("YQ=="), None); // one line
        assert_eq!(trusted_comment("YQphCmIKYw=="), None); // "a", "a", "b", "c": the third line has no prefix
        assert_eq!(trusted_comment("dW50cnVzdGVkIGNvbW1lbnQ6IHgKc2lnCm5vdCBhIHRydXN0ZWQgY29tbWVudApnbG9iYWwK"), None); // third line "not a trusted comment"
    }

    #[test]
    fn the_announced_version_must_be_the_one_the_installer_was_signed_as() {
        let by_name = trusted_comment(SIG_FILE_NAME);
        let with_version = trusted_comment(SIG_WITH_VERSION);
        assert_eq!(check_signed_release(by_name.as_deref(), "0.3.0"), Ok(()));
        assert_eq!(check_signed_release(with_version.as_deref(), "0.3.0"), Ok(()));
        // An old installer paired with a high announced version.
        let old = check_signed_release(by_name.as_deref(), "9.9.9").unwrap_err();
        assert!(matches!(old, Reason::SignedVersionMismatch { .. }));
        assert!(old.sentence().contains("9.9.9") && old.sentence().contains("0.3.0"));
        assert!(matches!(check_signed_release(with_version.as_deref(), "0.3.1"), Err(Reason::SignedVersionMismatch { .. })));
        // The version has to stand between underscores in the signed name: 0.3.0 is not inside 10.3.0 or 0.3.01.
        for comment in ["file:Aqua Assistant_10.3.0_x64-setup.exe", "file:Aqua Assistant_0.3.01_x64-setup.exe", "file:Aqua Assistant_0.3.0.exe", "file:Aqua Assistant x64-setup.exe"] {
            assert!(matches!(check_signed_release(Some(comment), "0.3.0"), Err(Reason::SignedVersionMismatch { .. })), "{comment}");
        }
        assert_eq!(check_signed_release(Some("timestamp:1\tfile:Aqua Assistant_0.3.0-rc.1_x64-setup.exe"), "0.3.0-rc.1"), Ok(()));
        // A signed version in the newer form may carry a leading v.
        assert_eq!(check_signed_release(Some("version:v0.3.0"), "0.3.0"), Ok(()));
        // Nothing to check against, or no comment at all: refused.
        assert_eq!(check_signed_release(Some("timestamp:1791417600"), "0.3.0"), Err(Reason::SignedVersionMissing));
        assert_eq!(check_signed_release(None, "0.3.0"), Err(Reason::SignedVersionMissing));
        // An unreadable announced version or signed version is refused too.
        assert!(matches!(check_signed_release(by_name.as_deref(), "latest"), Err(Reason::Unreadable { .. })));
        assert!(matches!(check_signed_release(Some("version:next"), "0.3.0"), Err(Reason::Unreadable { .. })));
    }

    #[test]
    fn the_saved_state_round_trips_and_a_damaged_one_pauses_updates() {
        assert_eq!(state_text("0.3.0").as_deref(), Some("{\"highest_version\": \"0.3.0\"}\n"));
        assert_eq!(read_state(&state_text("0.3.0").unwrap()), Ok("0.3.0".to_string()));
        assert_eq!(read_state(&state_text("1.0.0-rc.1").unwrap()), Ok("1.0.0-rc.1".to_string()));
        assert_eq!(read_state("{ \"highest_version\" :  \"0.3.0\" , \"other\": 1 }"), Ok("0.3.0".to_string()));
        // Only a readable version is ever written.
        assert_eq!(state_text("latest"), None);
        assert_eq!(state_text("0.3.0\", \"x\": \""), None);
        for damaged in ["", "{}", "{\"highest_version\": 3}", "{\"highest_version\": \"abc\"}", "{\"highest_version\" \"0.3.0\"}", "{\"highest_version\": \"0.3.0", "not json", "{\"highest_version\": \"\"}"] {
            assert_eq!(read_state(damaged), Err(Reason::StateDamaged), "{damaged:?}");
        }
        assert!(Reason::StateDamaged.sentence().contains("update_state.json"));
    }

    #[test]
    fn a_walk_through_the_life_of_an_installation() {
        // First run: nothing saved, the update to 0.2.0 is accepted and its version is what gets saved.
        let m020 = manifest("0.2.0", Some(EXPIRES));
        assert_eq!(decide(&m020, &context("0.1.0", None)), Verdict::Accept);
        let saved = state_text("0.2.0").unwrap();
        // The app now runs 0.2.0. The same manifest is "up to date", not a warning.
        let saved_version = read_state(&saved).unwrap();
        let after = refused(&m020, &context("0.2.0", Some(&saved_version)));
        assert!(!after.is_warning());
        // Someone puts the old 0.1.5 manifest back: it is not an update to the person on 0.2.0, whichever check names it.
        let m015 = manifest("0.1.5", Some(EXPIRES));
        assert!(matches!(refused(&m015, &context("0.2.0", Some(&saved_version))), Reason::Rollback { .. }));
        // The person reinstalls 0.1.0 by hand. The saved state still stops 0.1.5 (older than what was accepted) and
        // allows 0.2.0 again.
        assert!(matches!(refused(&m015, &context("0.1.0", Some(&saved_version))), Reason::Rollback { .. }));
        assert_eq!(decide(&m020, &context("0.1.0", Some(&saved_version))), Verdict::Accept);
    }
}
