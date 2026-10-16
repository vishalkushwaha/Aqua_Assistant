//! A repository's own `.aqua` folder, used only after the person has trusted it by the hash of its files (card OA11-01; task EX-06 of the OpenCode audit).
//!
//! A cloned repository can carry skills, commands, agents, hooks, MCP servers, formatters and tools of its own, and every one of them is a way for whoever wrote the repository to speak to
//! the agent, or to run a program on this machine. So **everything in a pack is data until the person has trusted it**: nothing is parsed, listed, loaded or run from it before then. The
//! files are of two kinds, and each kind is trusted on its own:
//!
//! * **Text** (`skills/<name>/SKILL.md` and up to ten other files beside it, `commands/<name>.md`, `agents/<name>.md`): text for the prompt. It cannot widen what the agent may do.
//!   A line of a command that begins with `` !` `` is a step, which is a call of the tool host: the steps of a repository's command are run as a script's calls, so each one asks the person
//!   every time (`api_gateway::server::step_caller`), and the page that shows the pack shows the beginning of each command file and all its steps ([`command_view`]), so that the person has
//!   read what they trust.
//! * **Code** (`hooks.json`, `mcp.json`, `format.json`, `tools.json`): it names programs to run. Trusting it needs the person to have seen the whole text (the route that shows the pack
//!   returns it).
//!
//! What is read is exactly this and no more: names are `[a-z0-9_-]{1,64}`, at most 200 files of at most 64 KiB (16 KiB for the other files of a skill), no link and no reparse point
//! (a junction), no two names that differ only in case, and text only. Anything else in the folder (`policy.json`, `fs.json`, `term*.json`, `http.json`, `db.json`, `ssh.json` ...) is
//! **never read**: a repository cannot change what Aqua allows, and a warning says what was left out.
//!
//! The files are read **once**, into memory (a [`Pack`]), and what is hashed is what is used: there is no second read in which a file could have changed. The trust record
//! `data/tools/trusted_packs.json` holds, for each project folder and each kind, an HMAC-SHA-256 tag (key: the trust key of the credential store, `audit_key::trust_key`) over the sorted
//! list of `<kind>/<path>` and the SHA-256 of each file, and the folder it is for. Whoever can only write the file cannot make a tag; with no key nothing is trusted. A kind whose files
//! differ from what was trusted in any way (a byte, a file added or removed) is [`Standing::Changed`] and is not used until the person trusts it again.
//!
//! Only the person trusts: the routes need the UI token, the voice and the API token are refused, and `aqua-cli pack trust` asks at a terminal.

use crate::project_instructions::{hash_of, key_of, LEVELS_UP};
use aqua_tool_host::hex_lower;
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The folder of a pack, in a project's folder.
pub const DIR: &str = ".aqua";
/// The most files one pack holds.
pub const MAX_FILES: usize = 200;
/// The biggest file of a pack.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
/// How many other files a skill may have beside its `SKILL.md`, and how big each may be.
pub const MAX_SIBLINGS: usize = 10;
pub const MAX_SIBLING_BYTES: usize = 16 * 1024;
/// The version of the trust record.
pub const RECORD_VERSION: u32 = 2;
const TAG_PREFIX: &str = "hmac:";
const MESSAGE_HEAD: &str = "aqua-pack-v1\n";
/// The code files of a pack.
pub const CODE_FILES: [&str; 4] = ["hooks.json", "mcp.json", "format.json", "tools.json"];

/// The two kinds of file in a pack, trusted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Text,
    Code,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Text, Kind::Code];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Code => "code",
        }
    }

    pub fn parse(word: &str) -> Option<Kind> {
        match word.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Kind::Text),
            "code" => Some(Kind::Code),
            _ => None,
        }
    }
}

/// One file of a pack, as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFile {
    pub kind: Kind,
    /// The path inside `.aqua`, with forward slashes (`skills/deploy/SKILL.md`).
    pub rel: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// What was read of a `.aqua` folder: the files that are allowed, and what was left out and why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pack {
    pub dir: PathBuf,
    pub files: Vec<PackFile>,
    pub warnings: Vec<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A file beside a skill's `SKILL.md`: letters, digits, `.`, `_`, `-`; not hidden.
fn valid_sibling(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.starts_with('.') && !name.ends_with('.') && name != "SKILL.md" && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Names that must never come from a repository: what Aqua allows, the folders it may touch, the terminal, the network, databases, machines.
fn is_security_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "policy.json" | "fs.json" | "http.json" | "db.json" | "ssh.json" | "permissions.json" | "remote_model.json" | "repos.json" | "docker.json" | "k8s.json" | "iac.json" | "cloud.json" | "forge.json" | "mon.json")
        || (n.starts_with("term") && n.ends_with(".json"))
}

// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Finding and reading a pack
// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Where a pack was looked for and what was seen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    /// The folder that holds `.aqua` (the project's folder).
    pub root: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// The nearest `.aqua` folder at or above `start`, at most four levels up, never above `limit` (the root of the folders Aqua works in): the way `project_instructions::standing` looks
/// for `AGENTS.md`. A `.aqua` that is a link or a junction ends the search with a warning (where it leads is not known); one that is a file is no pack.
pub fn find(start: &Path, limit: &Path) -> Found {
    let mut warnings = Vec::new();
    let mut dir = start.to_path_buf();
    for _ in 0..=LEVELS_UP {
        let candidate = dir.join(DIR);
        match std::fs::symlink_metadata(&candidate) {
            Ok(m) if aqua_tool_host::fsutil::is_link(&m) => {
                warnings.push(format!("{} is a link, which is not read", candidate.display()));
                return Found { root: None, warnings };
            }
            Ok(m) if m.is_dir() => return Found { root: Some(dir), warnings },
            Ok(_) => warnings.push(format!("{} is a file, not a folder, so it is no pack", candidate.display())),
            Err(_) => {}
        }
        let at_limit = key_of(&dir) == key_of(limit);
        match dir.parent() {
            Some(p) if !at_limit && aqua_native_tools::fs::guard::within(p, limit) => dir = p.to_path_buf(),
            _ => break,
        }
    }
    Found { root: None, warnings }
}

struct Scanner {
    files: Vec<PackFile>,
    warnings: Vec<String>,
    seen: BTreeSet<String>,
    full: bool,
}

impl Scanner {
    fn warn(&mut self, text: String) {
        if self.warnings.len() < 100 {
            self.warnings.push(text);
        }
    }

    /// Reads one file and keeps it when it is allowed. Whether it was kept.
    fn add(&mut self, kind: Kind, rel: String, path: &Path, limit: usize) -> bool {
        if self.files.len() >= MAX_FILES {
            if !self.full {
                self.full = true;
                self.warn(format!("the pack has more than {MAX_FILES} files; the rest were left out"));
            }
            return false;
        }
        if !self.seen.insert(rel.to_lowercase()) {
            self.warn(format!("{rel} was left out: another file has the same name in other letters"));
            return false;
        }
        match read_regular(path, limit) {
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Ok(_) => {
                    let sha256 = hash_of(&bytes);
                    self.files.push(PackFile { kind, rel, bytes, sha256 });
                    true
                }
                Err(_) => {
                    self.warn(format!("{rel} was left out: it is not text"));
                    false
                }
            },
            Err(why) => {
                self.warn(format!("{rel} was left out: {why}"));
                false
            }
        }
    }
}

/// A plain file of at most `limit` bytes, read once. A link, a junction or a folder is refused, and so is a file that turns out bigger than it said.
fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("it cannot be read ({e})"))?;
    if aqua_tool_host::fsutil::is_link(&meta) {
        return Err("it is a link, which is not read".into());
    }
    if !meta.is_file() {
        return Err("it is not a plain file".into());
    }
    if meta.len() > limit as u64 {
        return Err(format!("it is larger than {} KiB", limit / 1024));
    }
    let mut buf = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(limit as u64 + 1).read_to_end(&mut buf)).map_err(|e| format!("it cannot be read ({e})"))?;
    if buf.len() > limit {
        return Err(format!("it is larger than {} KiB", limit / 1024));
    }
    Ok(buf)
}

/// The entries of a folder by name: (name, path, metadata without following a link).
fn entries(dir: &Path) -> Vec<(String, PathBuf, std::fs::Metadata)> {
    let mut out: Vec<(String, PathBuf, std::fs::Metadata)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            Some((e.file_name().to_string_lossy().to_string(), e.path(), meta))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Reads a `.aqua` folder: the files that are allowed, once each, and a warning for everything else.
pub fn scan(dir: &Path) -> Pack {
    let mut s = Scanner { files: Vec::new(), warnings: Vec::new(), seen: BTreeSet::new(), full: false };
    for (name, path, meta) in entries(dir) {
        if aqua_tool_host::fsutil::is_link(&meta) {
            s.warn(format!("{name} is a link and was left out"));
            continue;
        }
        match name.as_str() {
            n if CODE_FILES.contains(&n) && meta.is_file() => {
                s.add(Kind::Code, name.clone(), &path, MAX_FILE_BYTES);
            }
            "skills" if meta.is_dir() => scan_skills(&mut s, &path),
            "commands" | "agents" if meta.is_dir() => scan_markdown(&mut s, &name, &path),
            // The folder Aqua puts the worktrees of its coding runs in (card OA7-05): working folders of the same repository, never part of a pack, never read, and no warning about them.
            "worktrees" if meta.is_dir() => {}
            n if is_security_file(n) => s.warn(format!("{name} is never read from a repository: what Aqua allows is not something a project can change")),
            _ => s.warn(format!("{name} is not part of a pack and was left out (a pack has skills, commands, agents, hooks.json, mcp.json, format.json and tools.json)")),
        }
    }
    Pack { dir: dir.to_path_buf(), files: s.files, warnings: s.warnings }
}

/// `commands/<name>.md` and `agents/<name>.md`.
fn scan_markdown(s: &mut Scanner, folder: &str, dir: &Path) {
    for (name, path, meta) in entries(dir) {
        let rel = format!("{folder}/{name}");
        if aqua_tool_host::fsutil::is_link(&meta) {
            s.warn(format!("{rel} is a link and was left out"));
        } else if meta.is_file() && name.strip_suffix(".md").is_some_and(valid_name) {
            s.add(Kind::Text, rel, &path, MAX_FILE_BYTES);
        } else {
            s.warn(format!("{rel} was left out: a name here is `<name>.md` with <name> of a-z, 0-9, - and _"));
        }
    }
}

/// `skills/<name>/SKILL.md` and up to [`MAX_SIBLINGS`] other files in that folder.
fn scan_skills(s: &mut Scanner, dir: &Path) {
    for (name, path, meta) in entries(dir) {
        let rel = format!("skills/{name}");
        if aqua_tool_host::fsutil::is_link(&meta) {
            s.warn(format!("{rel} is a link and was left out"));
            continue;
        }
        if !meta.is_dir() || !valid_name(&name) {
            s.warn(format!("{rel} was left out: a skill is a folder named with a-z, 0-9, - and _"));
            continue;
        }
        if !s.add(Kind::Text, format!("{rel}/SKILL.md"), &path.join("SKILL.md"), MAX_FILE_BYTES) {
            continue;
        }
        let mut siblings = 0;
        for (file, file_path, file_meta) in entries(&path) {
            if file == "SKILL.md" {
                continue;
            }
            let file_rel = format!("{rel}/{file}");
            if aqua_tool_host::fsutil::is_link(&file_meta) || !file_meta.is_file() {
                s.warn(format!("{file_rel} was left out: only plain files beside SKILL.md are read"));
            } else if !valid_sibling(&file) {
                s.warn(format!("{file_rel} was left out: its name is not letters, digits, . _ and -"));
            } else if siblings >= MAX_SIBLINGS {
                s.warn(format!("{file_rel} was left out: a skill may have {MAX_SIBLINGS} other files"));
            } else if s.add(Kind::Text, file_rel, &file_path, MAX_SIBLING_BYTES) {
                siblings += 1;
            }
        }
    }
}

impl Pack {
    /// The files of one kind, in the order of their paths.
    pub fn of_kind(&self, kind: Kind) -> Vec<&PackFile> {
        let mut v: Vec<&PackFile> = self.files.iter().filter(|f| f.kind == kind).collect();
        v.sort_by(|a, b| a.rel.cmp(&b.rel));
        v
    }

    /// The list that is hashed and signed: a line `<kind>/<path>\t<sha256>` for each file of the kind, by path.
    pub fn listing(&self, kind: Kind) -> String {
        self.of_kind(kind).iter().map(|f| format!("{}/{}\t{}\n", kind.as_str(), f.rel, f.sha256)).collect()
    }

    /// A short name for what the files of a kind are now, with no key: what the trust card shows and what the person's yes is tied to.
    pub fn digest(&self, kind: Kind) -> String {
        hash_of(self.listing(kind).as_bytes())
    }

    /// The bytes of one file, by its path inside `.aqua`.
    pub fn file(&self, rel: &str) -> Option<&PackFile> {
        self.files.iter().find(|f| f.rel == rel)
    }
}

// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The record of what is trusted
// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    /// `<kind>/<path>` -> sha256 at the time of trust. Only to say **what** changed; it decides nothing (the tag does), so a file writer who edits it gains nothing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    files: BTreeMap<String, String>,
}

impl Entry {
    fn tag(&self, kind: Kind) -> Option<&String> {
        match kind {
            Kind::Text => self.text.as_ref(),
            Kind::Code => self.code.as_ref(),
        }
    }

    fn set_tag(&mut self, kind: Kind, tag: Option<String>) {
        match kind {
            Kind::Text => self.text = tag,
            Kind::Code => self.code = tag,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    #[serde(default)]
    version: u32,
    /// Folder (lower-cased, forward slashes: `project_instructions::key_of`) -> what is trusted there.
    #[serde(default)]
    packs: BTreeMap<String, Entry>,
}

pub fn trust_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join("trusted_packs.json")
}

/// For reading: a file that cannot be read, or is not of this version, trusts nothing.
fn load(data_dir: &Path) -> Record {
    std::fs::read_to_string(trust_path(data_dir)).ok().and_then(|t| serde_json::from_str::<Record>(t.trim_start_matches('\u{feff}')).ok()).filter(|r| r.version == RECORD_VERSION).unwrap_or_default()
}

/// For changing: a file that is not there is an empty record, but one that cannot be read, is damaged or is of another version is an error, so that changing one decision does not write
/// back a record that has lost the others.
fn load_for_change(data_dir: &Path) -> Result<Record, String> {
    let path = trust_path(data_dir);
    match aqua_tool_host::fsutil::read_json_strict::<Record>(&path) {
        Ok(None) => Ok(Record { version: RECORD_VERSION, packs: BTreeMap::new() }),
        Ok(Some(r)) if r.version == RECORD_VERSION => Ok(r),
        Ok(Some(r)) => Err(format!("{} is of version {}, not {RECORD_VERSION}. No trust decision was changed; move it aside to start again (every pack then has to be trusted again)", path.display(), r.version)),
        Err(e) => Err(format!("{} {e}. No trust decision was changed, so the others are not lost. Repair the file, or move it aside to start again (every pack then has to be trusted again)", path.display())),
    }
}

fn save(data_dir: &Path, r: &Record) -> Result<(), String> {
    let text = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    aqua_tool_host::fsutil::write_atomic_durable(&trust_path(data_dir), text.as_bytes()).map_err(|e| format!("the trust record could not be written: {e}"))
}

fn message(root_key: &str, listing: &str) -> String {
    format!("{MESSAGE_HEAD}root\t{root_key}\n{listing}")
}

fn tag_of(key: &[u8], root_key: &str, listing: &str) -> String {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    format!("{TAG_PREFIX}{}", hex_lower(hmac::sign(&k, message(root_key, listing).as_bytes()).as_ref()))
}

/// Is `stored` the tag of this listing for this folder? Compared by `ring`, in constant time. Anything that is not `hmac:` and 64 hex digits is no tag.
fn tag_matches(key: &[u8], root_key: &str, listing: &str, stored: &str) -> bool {
    let Some(bytes) = stored.strip_prefix(TAG_PREFIX).and_then(unhex).filter(|b| b.len() == 32) else { return false };
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), message(root_key, listing).as_bytes(), &bytes).is_ok()
}

/// Where one kind of a pack stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// The pack has no file of this kind.
    NoFile,
    /// Files are there and the person has not trusted them (or there is no key to check a trust by).
    NotTrusted,
    /// The files are exactly what the person trusted.
    Trusted,
    /// The person trusted this kind and the files are not what was trusted: what was added, removed and changed (as the record says it was; the tag decides, not this).
    Changed { added: Vec<String>, removed: Vec<String>, changed: Vec<String> },
}

impl Standing {
    pub fn word(&self) -> &'static str {
        match self {
            Standing::NoFile => "no_file",
            Standing::NotTrusted => "not_trusted",
            Standing::Trusted => "trusted",
            Standing::Changed { .. } => "changed",
        }
    }

    pub fn is_trusted(&self) -> bool {
        matches!(self, Standing::Trusted)
    }
}

/// The standing of the two kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standings {
    pub text: Standing,
    pub code: Standing,
}

impl Standings {
    pub fn of(&self, kind: Kind) -> &Standing {
        match kind {
            Kind::Text => &self.text,
            Kind::Code => &self.code,
        }
    }
}

fn standing_in(record: &Record, key: Option<&[u8]>, root_key: &str, pack: &Pack, kind: Kind) -> Standing {
    let files = pack.of_kind(kind);
    if files.is_empty() {
        return Standing::NoFile;
    }
    let (Some(key), Some(entry)) = (key, record.packs.get(root_key)) else { return Standing::NotTrusted };
    let Some(stored) = entry.tag(kind) else { return Standing::NotTrusted };
    if tag_matches(key, root_key, &pack.listing(kind), stored) {
        return Standing::Trusted;
    }
    // A tag that is not a tag at all (a plain hash, a forged line) is not "changed": it is nothing the person ever said.
    if !stored.starts_with(TAG_PREFIX) || unhex(&stored[TAG_PREFIX.len()..]).is_none_or(|b| b.len() != 32) {
        return Standing::NotTrusted;
    }
    let prefix = format!("{}/", kind.as_str());
    let before: BTreeMap<&str, &str> = entry.files.iter().filter_map(|(k, v)| k.strip_prefix(&prefix).map(|r| (r, v.as_str()))).collect();
    let now: BTreeMap<&str, &str> = files.iter().map(|f| (f.rel.as_str(), f.sha256.as_str())).collect();
    let added = now.keys().filter(|k| !before.contains_key(*k)).map(|k| k.to_string()).collect();
    let removed = before.keys().filter(|k| !now.contains_key(*k)).map(|k| k.to_string()).collect();
    let changed = now.iter().filter(|(k, v)| before.get(*k).is_some_and(|b| b != *v)).map(|(k, _)| k.to_string()).collect();
    Standing::Changed { added, removed, changed }
}

/// Where each kind of `pack` stands for the project folder `root` (the folder that holds `.aqua`). With no `key` (the credential store has none, or cannot say) nothing is trusted.
pub fn standings(data_dir: &Path, key: Option<&[u8]>, root: &Path, pack: &Pack) -> Standings {
    let record = load(data_dir);
    let root_key = key_of(root);
    Standings { text: standing_in(&record, key, &root_key, pack, Kind::Text), code: standing_in(&record, key, &root_key, pack, Kind::Code) }
}

/// Why trusting was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// The person's yes was for files that are no longer these (`expect` did not match): look again.
    Changed(String),
    /// Nothing to trust, or no key, or a bad request.
    Invalid(String),
    /// The record could not be read or written.
    Record(String),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::Changed(s) | TrustError::Invalid(s) | TrustError::Record(s) => f.write_str(s),
        }
    }
}

/// Trusts the `kinds` of `pack` as they are now, for the project folder `root`. `expect` is, for any kind, the [`Pack::digest`] of the files the person was shown: when it is not what
/// the files are now, nothing is trusted (the files changed between the look and the yes). A kind with no file cannot be trusted.
pub fn trust(data_dir: &Path, key: &[u8], root: &Path, pack: &Pack, kinds: &[Kind], expect: &BTreeMap<String, String>) -> Result<(), TrustError> {
    if kinds.is_empty() {
        return Err(TrustError::Invalid("say which kinds to trust: text, code".into()));
    }
    let mut record = load_for_change(data_dir).map_err(TrustError::Record)?;
    let root_key = key_of(root);
    let entry = record.packs.entry(root_key.clone()).or_default();
    for kind in kinds {
        let files = pack.of_kind(*kind);
        if files.is_empty() {
            return Err(TrustError::Invalid(format!("this pack has no {} files to trust", kind.as_str())));
        }
        if expect.get(kind.as_str()).is_some_and(|want| *want != pack.digest(*kind)) {
            return Err(TrustError::Changed(format!("the {} files changed since you looked at them, so nothing was trusted: look at them again", kind.as_str())));
        }
        entry.set_tag(*kind, Some(tag_of(key, &root_key, &pack.listing(*kind))));
        let prefix = format!("{}/", kind.as_str());
        entry.files.retain(|k, _| !k.starts_with(&prefix));
        for f in files {
            entry.files.insert(format!("{prefix}{}", f.rel), f.sha256.clone());
        }
    }
    record.version = RECORD_VERSION;
    save(data_dir, &record).map_err(TrustError::Record)
}

/// Stops trusting the `kinds` of the pack of `root` (all of them when `kinds` is empty). Whether there was anything to stop.
pub fn untrust(data_dir: &Path, root: &Path, kinds: &[Kind]) -> Result<bool, String> {
    let mut record = load_for_change(data_dir)?;
    let root_key = key_of(root);
    let Some(entry) = record.packs.get_mut(&root_key) else { return Ok(false) };
    let kinds: Vec<Kind> = if kinds.is_empty() { Kind::ALL.to_vec() } else { kinds.to_vec() };
    let mut had = false;
    for kind in kinds {
        had |= entry.tag(kind).is_some();
        entry.set_tag(kind, None);
        let prefix = format!("{}/", kind.as_str());
        entry.files.retain(|k, _| !k.starts_with(&prefix));
    }
    if entry.text.is_none() && entry.code.is_none() {
        record.packs.remove(&root_key);
    }
    save(data_dir, &record)?;
    Ok(had)
}

// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// What the routes and the terminal show
// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// How much of a command file the trust view shows (characters): the beginning, which a person reads. The steps of the whole file are shown beside it, so that a long file cannot hide a
/// step after its first part.
pub const COMMAND_PREVIEW_CHARS: usize = 2_000;
/// The most steps and the most characters of one step that the trust view lists for a command (a command has at most four steps; a longer one is refused when it is read).
const PREVIEW_STEPS: usize = 8;
const PREVIEW_STEP_CHARS: usize = 300;

/// Is this file of the pack a command (`commands/<name>.md`, as `commands::Commands::from_pack` reads them)?
fn is_command_file(rel: &str) -> bool {
    rel.strip_prefix("commands/").and_then(|r| r.strip_suffix(".md")).is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

/// What the person is shown of a command file before they trust it: the first [`COMMAND_PREVIEW_CHARS`] characters, whether there is more, and every step of the whole file. Control
/// characters are shown as `?` (a repository's text must not be able to move the cursor of a terminal or hide what comes before it); a step is cut at `PREVIEW_STEP_CHARS` and at most
/// `PREVIEW_STEPS` are listed. `None` for a file that is not a command.
pub fn command_view(f: &PackFile) -> Option<(String, bool, Vec<String>)> {
    if !is_command_file(&f.rel) {
        return None;
    }
    let whole = String::from_utf8_lossy(&f.bytes).replace("\r\n", "\n");
    let plain = |s: &str| s.chars().map(|c| if c.is_control() && !matches!(c, '\n' | '\t') { '?' } else { c }).collect::<String>();
    let head = plain(&whole.chars().take(COMMAND_PREVIEW_CHARS).collect::<String>());
    let cut = whole.chars().nth(COMMAND_PREVIEW_CHARS).is_some();
    let mut steps = Vec::new();
    let mut rest = whole.as_str();
    while let Some(at) = rest.find("!`") {
        let after = &rest[at + 2..];
        let Some(end) = after.find('`') else { break };
        if steps.len() < PREVIEW_STEPS {
            steps.push(plain(&after[..end].trim().chars().take(PREVIEW_STEP_CHARS).collect::<String>()));
        }
        rest = &after[end + 1..];
    }
    Some((head, cut, steps))
}

fn kind_view(pack: &Pack, standings: &Standings, kind: Kind, with_text: bool) -> Value {
    let files: Vec<Value> = pack
        .of_kind(kind)
        .iter()
        .map(|f| {
            let mut v = json!({ "path": f.rel, "bytes": f.bytes.len(), "sha256": f.sha256 });
            if with_text {
                v["text"] = json!(String::from_utf8_lossy(&f.bytes));
            }
            // A command file: its beginning and its steps, so that the person sees what they trust (`text`, the whole file, is for the code kind only).
            if let Some((text, cut, steps)) = command_view(f) {
                v["command_text"] = json!(text);
                v["command_text_cut"] = json!(cut);
                v["command_steps"] = json!(steps);
            }
            v
        })
        .collect();
    let standing = standings.of(kind);
    let mut v = json!({ "standing": standing.word(), "digest": pack.digest(kind), "files": files });
    if let Standing::Changed { added, removed, changed } = standing {
        v["changes"] = json!({ "added": added, "removed": removed, "changed": changed });
    }
    v
}

/// The pack as a page shows it: for each kind its standing, the digest the person's yes is tied to, and the files (size and hash; for the **code** kind the whole text of each, so that
/// what is trusted is what was read). `root` is the project's folder.
pub fn view(root: &Path, pack: &Pack, standings: &Standings) -> Value {
    json!({
        "folder": root.to_string_lossy(),
        "pack_dir": pack.dir.to_string_lossy(),
        "kinds": {
            "text": kind_view(pack, standings, Kind::Text, false),
            "code": kind_view(pack, standings, Kind::Code, true),
        },
        "warnings": pack.warnings,
    })
}

/// How many project folders have something recorded as trusted.
pub fn recorded_count(data_dir: &Path) -> usize {
    load(data_dir).packs.len()
}

/// Every pack the person has trusted, and whether it still is (for a settings page). The folders are read again.
pub fn list(data_dir: &Path, key: Option<&[u8]>) -> Value {
    let record = load(data_dir);
    json!(record
        .packs
        .keys()
        .map(|k| {
            let root = Path::new(k);
            let pack = scan(&root.join(DIR));
            let st = standings_with(&record, key, k, &pack);
            json!({ "folder": k, "text": st.text.word(), "code": st.code.word() })
        })
        .collect::<Vec<_>>())
}

fn standings_with(record: &Record, key: Option<&[u8]>, root_key: &str, pack: &Pack) -> Standings {
    Standings { text: standing_in(record, key, root_key, pack, Kind::Text), code: standing_in(record, key, root_key, pack, Kind::Code) }
}

// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Import from another tool's folders
// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// What `aqua-cli pack import` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    /// Files written under `.aqua`.
    pub written: Vec<String>,
    /// Files that were not written because one with that name is there already, or could not be converted, each with the reason.
    pub skipped: Vec<String>,
    /// Things that were not imported because they run programs: they need a person to look at them.
    pub manual_review: Vec<String>,
}

fn normalise_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let mapped: String = lower.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' { c } else { '-' }).collect();
    mapped.trim_matches('-').chars().take(64).collect()
}

/// Writes a new file; an existing one is never replaced. Whether it was written.
fn write_new(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => f.write_all(bytes).map(|_| true).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The front matter of `text` with its `name:` line set to `name`; `None` when it has no such line.
fn with_name(text: &str, name: &str) -> Option<String> {
    let mut out = String::new();
    // 0: before the opening line of three dashes, 1: in the front matter, 2: after it.
    let mut state = 0u8;
    let mut done = false;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        let ending = &line[bare.len()..];
        match state {
            0 => {
                if bare.trim_start_matches('\u{feff}').trim_end() == "---" {
                    state = 1;
                } else if !bare.trim().is_empty() {
                    return None;
                }
                out.push_str(line);
            }
            1 => {
                if bare.trim_end() == "---" {
                    state = 2;
                    out.push_str(line);
                } else if !done && bare.starts_with("name:") {
                    out.push_str(&format!("name: {name}{ending}"));
                    done = true;
                } else {
                    out.push_str(line);
                }
            }
            _ => out.push_str(line),
        }
    }
    done.then_some(out)
}

fn convert_skills(source: &Path, target: &Path, report: &mut ImportReport) -> Result<(), String> {
    for (folder, path, meta) in entries(source) {
        let shown = format!("skills/{folder}");
        if aqua_tool_host::fsutil::is_link(&meta) || !meta.is_dir() {
            report.skipped.push(format!("{shown}: not a plain folder"));
            continue;
        }
        let name = normalise_name(&folder);
        if name.is_empty() {
            report.skipped.push(format!("{shown}: no usable name"));
            continue;
        }
        let bytes = match read_regular(&path.join("SKILL.md"), MAX_FILE_BYTES) {
            Ok(b) => b,
            Err(why) => {
                report.skipped.push(format!("{shown}: SKILL.md {why}"));
                continue;
            }
        };
        let Ok(mut text) = String::from_utf8(bytes) else {
            report.skipped.push(format!("{shown}: SKILL.md is not text"));
            continue;
        };
        match crate::front_matter::parse(&text) {
            Err(e) => {
                report.skipped.push(format!("{shown}: {e}"));
                continue;
            }
            Ok(doc) => {
                if doc.scalar("name").map(|n| n.trim().to_string()) != Some(name.clone()) {
                    match with_name(&text, &name) {
                        Some(fixed) => text = fixed,
                        None => {
                            report.skipped.push(format!("{shown}: it has no name line to make match the folder"));
                            continue;
                        }
                    }
                }
            }
        }
        let dest = target.join("skills").join(&name);
        match write_new(&dest.join("SKILL.md"), text.as_bytes())? {
            true => report.written.push(format!("skills/{name}/SKILL.md")),
            false => {
                report.skipped.push(format!("skills/{name}/SKILL.md: there is one already"));
                continue;
            }
        }
        let mut siblings = 0;
        for (file, file_path, file_meta) in entries(&path) {
            if file == "SKILL.md" {
                continue;
            }
            if aqua_tool_host::fsutil::is_link(&file_meta) || !file_meta.is_file() || !valid_sibling(&file) {
                report.skipped.push(format!("{shown}/{file}: only plain files with simple names are imported"));
            } else if siblings >= MAX_SIBLINGS {
                report.skipped.push(format!("{shown}/{file}: a skill may have {MAX_SIBLINGS} other files"));
            } else {
                match read_regular(&file_path, MAX_SIBLING_BYTES).and_then(|b| String::from_utf8(b).map_err(|_| "it is not text".to_string())) {
                    Ok(t) => {
                        if write_new(&dest.join(&file), t.as_bytes())? {
                            report.written.push(format!("skills/{name}/{file}"));
                            siblings += 1;
                        } else {
                            report.skipped.push(format!("skills/{name}/{file}: there is one already"));
                        }
                    }
                    Err(why) => report.skipped.push(format!("{shown}/{file}: {why}")),
                }
            }
        }
    }
    Ok(())
}

fn convert_markdown(folder: &str, source: &Path, target: &Path, report: &mut ImportReport) -> Result<(), String> {
    for (file, path, meta) in entries(source) {
        let shown = format!("{folder}/{file}");
        if aqua_tool_host::fsutil::is_link(&meta) {
            report.skipped.push(format!("{shown}: a link"));
            continue;
        }
        let Some(stem) = file.strip_suffix(".md").filter(|_| meta.is_file()) else {
            report.skipped.push(format!("{shown}: only <name>.md files are imported (a folder of commands is not)"));
            continue;
        };
        let name = normalise_name(stem);
        if name.is_empty() {
            report.skipped.push(format!("{shown}: no usable name"));
            continue;
        }
        match read_regular(&path, MAX_FILE_BYTES).and_then(|b| String::from_utf8(b).map_err(|_| "it is not text".to_string())) {
            Ok(text) => {
                if write_new(&target.join(folder).join(format!("{name}.md")), text.as_bytes())? {
                    report.written.push(format!("{folder}/{name}.md"));
                } else {
                    report.skipped.push(format!("{folder}/{name}.md: there is one already"));
                }
            }
            Err(why) => report.skipped.push(format!("{shown}: {why}")),
        }
    }
    Ok(())
}

/// Copies the commands, skills and agents of another tool's folders (`.claude`, `.opencode`) in `project` into `project/.aqua`, converted to the shape of a pack. It **never overwrites**
/// a file, **never writes** `hooks.json`, `mcp.json`, `format.json` or `tools.json` (they run programs: what the other tool had is listed for a person to look at), and trusts nothing:
/// what it wrote is data until the person trusts it. `from` is `claude`, `opencode`, or `None` for both.
pub fn import(project: &Path, from: Option<&str>) -> Result<ImportReport, String> {
    if !project.is_dir() {
        return Err(format!("{} is not a folder", project.display()));
    }
    let origins: &[&str] = match from.map(str::to_ascii_lowercase).as_deref() {
        None => &["claude", "opencode"],
        Some("claude") => &["claude"],
        Some("opencode") => &["opencode"],
        Some(other) => return Err(format!("--from is claude or opencode, not {other:?}")),
    };
    let target = project.join(DIR);
    if let Ok(m) = std::fs::symlink_metadata(&target) {
        if aqua_tool_host::fsutil::is_link(&m) || !m.is_dir() {
            return Err(format!("{} is a link or a file; it was not touched", target.display()));
        }
    }
    let mut report = ImportReport::default();
    let mut found = false;
    for origin in origins {
        let base = project.join(format!(".{origin}"));
        if !base.is_dir() {
            continue;
        }
        found = true;
        // OpenCode has used both the singular and the plural folder names.
        let (commands, skills, agents): (&[&str], &[&str], &[&str]) = if *origin == "claude" { (&["commands"], &["skills"], &["agents"]) } else { (&["command", "commands"], &["skill", "skills"], &["agent", "agents"]) };
        for sub in commands {
            convert_markdown("commands", &base.join(sub), &target, &mut report)?;
        }
        for sub in agents {
            convert_markdown("agents", &base.join(sub), &target, &mut report)?;
        }
        for sub in skills {
            convert_skills(&base.join(sub), &target, &mut report)?;
        }
        for (name, why) in [
            ("settings.json", "hooks and MCP servers of this file run programs"),
            ("settings.local.json", "hooks and MCP servers of this file run programs"),
            ("opencode.json", "its MCP servers, plugins and permissions run programs or change what is allowed"),
            ("opencode.jsonc", "its MCP servers, plugins and permissions run programs or change what is allowed"),
            ("plugins", "plugins run code"),
            ("plugin", "plugins run code"),
            ("tool", "custom tools run code"),
            ("tools", "custom tools run code"),
            ("hooks", "hooks run programs"),
        ] {
            if base.join(name).exists() {
                report.manual_review.push(format!(".{origin}/{name}: {why}; it was not imported (write hooks.json, mcp.json or tools.json of a pack by hand, and trust it as code)"));
            }
        }
    }
    let mcp = project.join(".mcp.json");
    if mcp.exists() && (from.is_none() || from.is_some_and(|f| f.eq_ignore_ascii_case("claude"))) {
        report.manual_review.push(".mcp.json: MCP servers run programs; it was not imported (write mcp.json of a pack by hand, and trust it as code)".into());
    }
    if !found {
        return Err(format!("there is no {} folder in {}", origins.iter().map(|o| format!(".{o}")).collect::<Vec<_>>().join(" or "), project.display()));
    }
    Ok(report)
}

// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The terminal: `aqua-cli pack ...`
// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------

fn the_pack_of(folder: &str) -> Result<(PathBuf, Pack), String> {
    let root = std::fs::canonicalize(folder).map_err(|e| format!("{folder}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let candidate = root.join(DIR);
    match std::fs::symlink_metadata(&candidate) {
        Ok(m) if m.is_dir() && !aqua_tool_host::fsutil::is_link(&m) => Ok((root.clone(), scan(&candidate))),
        _ => Err(format!("there is no {DIR} folder in {}", root.display())),
    }
}

fn words(standings: &Standings, pack: &Pack, kind: Kind) -> String {
    let n = pack.of_kind(kind).len();
    let note = match standings.of(kind) {
        Standing::Changed { added, removed, changed } => format!(" (added {}, removed {}, changed {})", added.len(), removed.len(), changed.len()),
        _ => String::new(),
    };
    format!("{}: {} file(s), {}{note}", kind.as_str(), n, standings.of(kind).word())
}

/// `aqua-cli pack import <dir> [--from claude|opencode]`, `pack status <dir>`, `pack trust <dir> [--kinds text,code]` and `pack untrust <dir> [--kinds ...]`. `key_for(create)` gives the trust key
/// (`create` only for the act of trusting); `confirm` shows a text and asks the person at the terminal for a yes: **trusting is refused without it**, and the caller must not give a
/// `confirm` that says yes by itself.
pub fn run_cli(data_dir: &Path, args: &[String], key_for: &dyn Fn(bool) -> Option<Vec<u8>>, confirm: &dyn Fn(&str) -> bool) -> Result<String, String> {
    let usage = "usage: aqua-cli pack import <dir> [--from claude|opencode] | status <dir> | trust <dir> [--kinds text,code] | untrust <dir> [--kinds text,code]";
    let sub = args.first().map(String::as_str).ok_or(usage)?;
    let folder = args.get(1).filter(|a| !a.starts_with("--")).ok_or(usage)?;
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let kinds: Vec<Kind> = match flag("--kinds") {
        Some(list) => list.split(',').map(|w| Kind::parse(w).ok_or_else(|| format!("--kinds is text, code or both, not {w:?}"))).collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    match sub {
        "import" => {
            let r = import(Path::new(folder), flag("--from").as_deref())?;
            let mut out = format!("Imported into {}:\n", Path::new(folder).join(DIR).display());
            for w in &r.written {
                out.push_str(&format!("  wrote   {w}\n"));
            }
            for s in &r.skipped {
                out.push_str(&format!("  skipped {s}\n"));
            }
            for m in &r.manual_review {
                out.push_str(&format!("  REVIEW  {m}\n"));
            }
            out.push_str("Nothing is trusted yet. Look at the files, then: aqua-cli pack trust <dir>\n");
            Ok(out)
        }
        "status" => {
            let (root, pack) = the_pack_of(folder)?;
            let key = key_for(false);
            let st = standings(data_dir, key.as_deref(), &root, &pack);
            let mut out = format!("Pack of {}\n  {}\n  {}\n", root.display(), words(&st, &pack, Kind::Text), words(&st, &pack, Kind::Code));
            for f in &pack.files {
                out.push_str(&format!("    [{}] {} ({} bytes)\n", f.kind.as_str(), f.rel, f.bytes.len()));
            }
            for w in &pack.warnings {
                out.push_str(&format!("  warning: {w}\n"));
            }
            if key.is_none() {
                out.push_str("  There is no trust key on this machine yet, so nothing is trusted.\n");
            }
            Ok(out)
        }
        "trust" => {
            let (root, pack) = the_pack_of(folder)?;
            let kinds = if kinds.is_empty() { Kind::ALL.into_iter().filter(|k| !pack.of_kind(*k).is_empty()).collect() } else { kinds };
            let mut shown = format!("Trust these files of {}?\n", root.display());
            for kind in &kinds {
                shown.push_str(&format!("\n== {} ({} file(s), digest {}) ==\n", kind.as_str(), pack.of_kind(*kind).len(), &pack.digest(*kind)[..16]));
                for f in pack.of_kind(*kind) {
                    shown.push_str(&format!("--- {} ({} bytes, sha256 {})\n", f.rel, f.bytes.len(), &f.sha256[..16]));
                    // Code is shown whole: it names programs to run.
                    if *kind == Kind::Code {
                        shown.push_str(&String::from_utf8_lossy(&f.bytes));
                        shown.push('\n');
                    }
                    // A command is shown from its beginning, and its steps in full: each step is a program that is put to the person when the command is started.
                    if let Some((text, cut, steps)) = command_view(f) {
                        shown.push_str(&text);
                        shown.push('\n');
                        if cut {
                            shown.push_str(&format!("[only the first {COMMAND_PREVIEW_CHARS} characters of this command are shown; its steps, all of them:]\n"));
                        }
                        for (n, step) in steps.iter().enumerate() {
                            shown.push_str(&format!("  step {}: {step}\n", n + 1));
                        }
                    }
                }
                if *kind == Kind::Code {
                    shown.push_str("\nCODE: these files name programs that will run on this machine while the pack is trusted. Read them above.\n");
                } else {
                    shown.push_str("\nTEXT: this is prompt text for the agent. It cannot widen what the agent may do.\n");
                }
            }
            if !confirm(&shown) {
                return Err("nothing was trusted".into());
            }
            let key = key_for(true).ok_or("there is no trust key and none could be made (the credential store could not be used), so nothing can be trusted")?;
            let expect: BTreeMap<String, String> = kinds.iter().map(|k| (k.as_str().to_string(), pack.digest(*k))).collect();
            trust(data_dir, &key, &root, &pack, &kinds, &expect).map_err(|e| e.to_string())?;
            Ok(format!("Trusted {} of {}. A file that changes after this makes that kind untrusted again.\n", kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(" and "), root.display()))
        }
        "untrust" => {
            let root = std::fs::canonicalize(folder).map_err(|e| format!("{folder}: {e}"))?;
            let had = untrust(data_dir, &root, &kinds)?;
            Ok(if had { format!("Stopped trusting the pack of {}.\n", root.display()) } else { format!("The pack of {} was not trusted.\n", root.display()) })
        }
        _ => Err(usage.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = &[7u8; 32];

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_pack_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::create_dir_all(d.join("ws/proj/.aqua")).unwrap();
        d
    }

    fn put(d: &Path, rel: &str, text: &str) {
        let p = d.join("ws/proj/.aqua").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn root(d: &Path) -> PathBuf {
        d.join("ws/proj")
    }

    fn pack_of(d: &Path) -> Pack {
        scan(&root(d).join(DIR))
    }

    fn no_expect() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    const SKILL: &str = "---\nname: deploy\ndescription: Deploy it\n---\nsteps";

    #[test]
    fn only_the_allowed_files_are_read_and_the_rest_is_named_in_a_warning() {
        let d = temp("allowed");
        put(&d, "skills/deploy/SKILL.md", SKILL);
        put(&d, "skills/deploy/checklist.md", "- one");
        put(&d, "commands/review.md", "review it");
        put(&d, "agents/plan.md", "plan it");
        put(&d, "hooks.json", r#"{"hooks": []}"#);
        put(&d, "mcp.json", "{}");
        put(&d, "policy.json", r#"{"mode": "full"}"#);
        put(&d, "fs.json", "{}");
        put(&d, "terminal.json", "{}");
        put(&d, "notes.txt", "x");
        put(&d, "commands/Bad Name.md", "x");
        put(&d, "skills/UPPER/SKILL.md", SKILL);
        put(&d, "skills/deploy/.hidden", "x");
        let pack = pack_of(&d);
        let rels: Vec<&str> = pack.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["agents/plan.md", "commands/review.md", "hooks.json", "mcp.json", "skills/deploy/SKILL.md", "skills/deploy/checklist.md"]);
        assert_eq!(pack.of_kind(Kind::Code).len(), 2);
        assert_eq!(pack.of_kind(Kind::Text).len(), 4);
        let w = pack.warnings.join("\n");
        for never in ["policy.json is never read", "fs.json is never read", "terminal.json is never read"] {
            assert!(w.contains(never), "{never} in:\n{w}");
        }
        assert!(w.contains("notes.txt is not part of a pack") && w.contains("Bad Name.md was left out") && w.contains("skills/UPPER was left out") && w.contains(".hidden was left out"), "{w}");
    }

    /// Card OA7-05: the folder Aqua puts the worktrees of its coding runs in is no part of a pack, nothing in it is read (a worktree is a checkout of the repository, which may hold a pack of
    /// its own), and it is not worth a warning; a file of that name is still an unknown file.
    #[test]
    fn the_folder_of_worktrees_is_not_read_and_not_warned_about() {
        let d = temp("worktrees");
        put(&d, "commands/review.md", "review it");
        put(&d, "worktrees/w1/.aqua/commands/other.md", "from another checkout");
        put(&d, "worktrees/w1/src/a.rs", "fn main() {}");
        let pack = pack_of(&d);
        let rels: Vec<&str> = pack.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["commands/review.md"], "nothing in the worktrees is read");
        assert!(pack.warnings.is_empty(), "{:?}", pack.warnings);
        let file = temp("worktrees_file");
        put(&file, "worktrees", "not a folder");
        assert!(pack_of(&file).warnings.iter().any(|w| w.contains("worktrees is not part of a pack")), "{:?}", pack_of(&file).warnings);
    }

    #[test]
    fn a_file_over_64_kib_a_binary_file_and_the_201st_file_are_left_out() {
        let d = temp("limits");
        put(&d, "commands/big.md", &"a".repeat(MAX_FILE_BYTES + 1));
        put(&d, "commands/exact.md", &"a".repeat(MAX_FILE_BYTES));
        std::fs::write(root(&d).join(".aqua/commands/binary.md"), [0xffu8, 0xfe, 0x00, 0x80]).unwrap();
        put(&d, "skills/deploy/SKILL.md", SKILL);
        put(&d, "skills/deploy/big.txt", &"a".repeat(MAX_SIBLING_BYTES + 1));
        for i in 0..12 {
            put(&d, &format!("skills/deploy/f{i:02}.txt"), "x");
        }
        let pack = pack_of(&d);
        let w = pack.warnings.join("\n");
        assert!(pack.file("commands/exact.md").is_some() && pack.file("commands/big.md").is_none(), "{w}");
        assert!(w.contains("commands/big.md was left out: it is larger than 64 KiB") && w.contains("commands/binary.md was left out: it is not text"), "{w}");
        assert!(w.contains("skills/deploy/big.txt was left out: it is larger than 16 KiB"), "{w}");
        let siblings = pack.files.iter().filter(|f| f.rel.starts_with("skills/deploy/") && f.rel != "skills/deploy/SKILL.md").count();
        assert_eq!(siblings, MAX_SIBLINGS, "ten other files at most: {w}");
        assert!(w.contains("a skill may have 10 other files"), "{w}");

        let many = temp("many");
        for i in 0..(MAX_FILES + 5) {
            put(&many, &format!("commands/c{i:03}.md"), "x");
        }
        let pack = pack_of(&many);
        assert_eq!(pack.files.len(), MAX_FILES);
        assert!(pack.warnings.iter().any(|w| w.contains("more than 200 files")), "{:?}", pack.warnings);
    }

    #[test]
    fn a_link_or_a_junction_is_never_read_and_a_dot_aqua_that_is_a_link_ends_the_search() {
        let d = temp("links");
        put(&d, "commands/real.md", "real");
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/secret.md"), "secret").unwrap();
        // A link to a file, and a link to a folder (a junction when the system does not allow symbolic links).
        #[cfg(windows)]
        let file_link = std::os::windows::fs::symlink_file(d.join("outside/secret.md"), root(&d).join(".aqua/commands/link.md")).is_ok();
        #[cfg(not(windows))]
        let file_link = std::os::unix::fs::symlink(d.join("outside/secret.md"), root(&d).join(".aqua/commands/link.md")).is_ok();
        let pack = pack_of(&d);
        assert!(pack.file("commands/real.md").is_some());
        if file_link {
            assert!(pack.file("commands/link.md").is_none() && pack.warnings.iter().any(|w| w.contains("link.md is a link")), "{:?}", pack.warnings);
        }
        // `.aqua` itself a link: another project whose `.aqua` leads to a folder outside.
        std::fs::create_dir_all(d.join("ws/other")).unwrap();
        #[cfg(windows)]
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(d.join("ws/other/.aqua")).arg(d.join("outside")).output().is_ok_and(|o| o.status.success());
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(d.join("outside"), d.join("ws/other/.aqua")).is_ok();
        if made {
            let found = find(&d.join("ws/other"), &d.join("ws"));
            assert_eq!(found.root, None, "a pack that is a link is not found, and the search does not go on to a parent");
            assert!(found.warnings.iter().any(|w| w.contains("is a link")), "{:?}", found.warnings);
            #[cfg(windows)]
            let _ = std::fs::remove_dir(d.join("ws/other/.aqua"));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_names_that_differ_only_in_case_are_one_name_and_the_second_is_left_out() {
        let d = temp("case");
        put(&d, "skills/deploy/SKILL.md", SKILL);
        put(&d, "skills/deploy/Notes.md", "one");
        put(&d, "skills/deploy/notes.md", "two");
        let pack = pack_of(&d);
        let notes: Vec<&str> = pack.files.iter().filter(|f| f.rel.to_lowercase() == "skills/deploy/notes.md").map(|f| f.rel.as_str()).collect();
        // On a file system that does not tell the two apart there is one file, on another the second is refused.
        assert_eq!(notes.len(), 1, "{notes:?} {:?}", pack.warnings);
    }

    #[test]
    fn the_nearest_pack_at_most_four_levels_up_and_never_above_the_root() {
        let d = temp("find");
        std::fs::create_dir_all(d.join("ws/proj/a/b/c/d/e")).unwrap();
        let ws = d.join("ws");
        assert_eq!(find(&root(&d), &ws).root, Some(root(&d)));
        assert_eq!(find(&root(&d).join("a/b"), &ws).root, Some(root(&d)));
        assert_eq!(find(&root(&d).join("a/b/c/d"), &ws).root, Some(root(&d)), "four levels up");
        assert_eq!(find(&root(&d).join("a/b/c/d/e"), &ws).root, None, "five levels up is too far");
        // A pack above the root is not looked at.
        std::fs::create_dir_all(d.join("ws/inner/deeper")).unwrap();
        std::fs::create_dir_all(d.join(".aqua")).unwrap();
        assert_eq!(find(&d.join("ws/inner/deeper"), &d.join("ws/inner")).root, None);
        // `.aqua` as a file is no pack, and a note says so.
        std::fs::write(d.join("ws/inner/.aqua"), "x").unwrap();
        let f = find(&d.join("ws/inner"), &d.join("ws"));
        assert_eq!(f.root, None);
        assert!(f.warnings.iter().any(|w| w.contains("is a file, not a folder")), "{:?}", f.warnings);
    }

    #[test]
    fn nothing_is_trusted_until_it_is_and_each_kind_is_trusted_on_its_own() {
        let d = temp("trust");
        put(&d, "skills/deploy/SKILL.md", SKILL);
        put(&d, "hooks.json", r#"{"hooks": []}"#);
        let data = d.join("data");
        let pack = pack_of(&d);
        let st = standings(&data, Some(KEY), &root(&d), &pack);
        assert_eq!((st.text.clone(), st.code.clone()), (Standing::NotTrusted, Standing::NotTrusted));
        trust(&data, KEY, &root(&d), &pack, &[Kind::Text], &no_expect()).unwrap();
        let st = standings(&data, Some(KEY), &root(&d), &pack);
        assert_eq!((st.text.clone(), st.code.clone()), (Standing::Trusted, Standing::NotTrusted), "text only: the code is still not trusted");
        trust(&data, KEY, &root(&d), &pack, &[Kind::Code], &no_expect()).unwrap();
        assert!(standings(&data, Some(KEY), &root(&d), &pack).code.is_trusted());
        // No key: nothing is trusted, whatever the record says.
        let st = standings(&data, None, &root(&d), &pack);
        assert_eq!((st.text.clone(), st.code.clone()), (Standing::NotTrusted, Standing::NotTrusted));
        // Another key (a forged record made without the real one): nothing is trusted.
        let st = standings(&data, Some(&[9u8; 32]), &root(&d), &pack);
        assert!(matches!(st.text, Standing::Changed { .. }), "{:?}", st.text);
        // A kind with no files cannot be trusted, and is not "trusted" either.
        let empty = temp("trust_empty");
        put(&empty, "commands/x.md", "x");
        let e = trust(&empty.join("data"), KEY, &root(&empty), &pack_of(&empty), &[Kind::Code], &no_expect()).unwrap_err();
        assert!(e.to_string().contains("no code files"), "{e}");
        assert_eq!(standings(&empty.join("data"), Some(KEY), &root(&empty), &pack_of(&empty)).code, Standing::NoFile);
    }

    #[test]
    fn one_byte_changed_after_trust_makes_that_kind_changed_and_says_which_file() {
        let d = temp("changed");
        put(&d, "skills/deploy/SKILL.md", SKILL);
        put(&d, "commands/review.md", "review it");
        put(&d, "hooks.json", r#"{"hooks": []}"#);
        let data = d.join("data");
        trust(&data, KEY, &root(&d), &pack_of(&d), &[Kind::Text, Kind::Code], &no_expect()).unwrap();
        assert!(standings(&data, Some(KEY), &root(&d), &pack_of(&d)).text.is_trusted());
        put(&d, "commands/review.md", "review it!");
        let st = standings(&data, Some(KEY), &root(&d), &pack_of(&d));
        assert_eq!(st.text, Standing::Changed { added: vec![], removed: vec![], changed: vec!["commands/review.md".into()] });
        assert!(st.code.is_trusted(), "the other kind is not touched");
        // A file added, one removed.
        put(&d, "commands/new.md", "n");
        std::fs::remove_file(root(&d).join(".aqua/commands/review.md")).unwrap();
        let st = standings(&data, Some(KEY), &root(&d), &pack_of(&d));
        assert_eq!(st.text, Standing::Changed { added: vec!["commands/new.md".into()], removed: vec!["commands/review.md".into()], changed: vec![] });
        // The code is changed by a change of line ends too: the hash is of the bytes.
        put(&d, "hooks.json", "{\"hooks\": []}\r\n");
        assert!(matches!(standings(&data, Some(KEY), &root(&d), &pack_of(&d)).code, Standing::Changed { .. }));
        // Trusting again, as it is now, makes it trusted.
        trust(&data, KEY, &root(&d), &pack_of(&d), &[Kind::Text, Kind::Code], &no_expect()).unwrap();
        let st = standings(&data, Some(KEY), &root(&d), &pack_of(&d));
        assert!(st.text.is_trusted() && st.code.is_trusted());
        // Untrusting.
        assert!(untrust(&data, &root(&d), &[Kind::Code]).unwrap());
        let st = standings(&data, Some(KEY), &root(&d), &pack_of(&d));
        assert!(st.text.is_trusted() && st.code == Standing::NotTrusted);
        assert!(untrust(&data, &root(&d), &[]).unwrap());
        assert!(!untrust(&data, &root(&d), &[]).unwrap(), "nothing left to withdraw");
        assert_eq!(load(&data).packs.len(), 0, "the entry is gone with its last kind");
    }

    #[test]
    fn a_record_somebody_wrote_without_the_key_trusts_nothing() {
        let d = temp("forged");
        put(&d, "hooks.json", r#"{"hooks": [{"event": "PreToolUse", "command": ["evil.exe"]}]}"#);
        let data = d.join("data");
        let pack = pack_of(&d);
        let listing = pack.listing(Kind::Code);
        let rk = key_of(&root(&d));
        std::fs::create_dir_all(data.join("tools")).unwrap();
        let write = |code: &str| {
            let rec = json!({"version": 2, "packs": {rk.clone(): {"code": code, "files": {"code/hooks.json": pack.files[0].sha256}}}});
            std::fs::write(trust_path(&data), rec.to_string()).unwrap();
        };
        // A plain hash of the listing, a tag made with a guessed key, a tag of another folder, a tag of nothing, and one that is not hex.
        for forged in [format!("sha256:{}", hash_of(listing.as_bytes())), tag_of(b"guess", &rk, &listing), tag_of(KEY, "e:/other/project", &listing), hash_of(listing.as_bytes()), "hmac:zz".to_string(), String::new()] {
            write(&forged);
            let st = standings(&data, Some(KEY), &root(&d), &pack);
            assert!(!st.code.is_trusted(), "{forged}: {:?}", st.code);
        }
        // The same record with the right tag is trusted (so the test is about the tag and not about the shape).
        write(&tag_of(KEY, &rk, &listing));
        assert!(standings(&data, Some(KEY), &root(&d), &pack).code.is_trusted());
        // The tag of one project copied into the record of another does not trust the other: the folder is in the message.
        let other = temp("forged_other");
        put(&other, "hooks.json", r#"{"hooks": [{"event": "PreToolUse", "command": ["evil.exe"]}]}"#);
        let rec = json!({"version": 2, "packs": {key_of(&root(&other)): {"code": tag_of(KEY, &rk, &listing)}}});
        std::fs::create_dir_all(other.join("data/tools")).unwrap();
        std::fs::write(trust_path(&other.join("data")), rec.to_string()).unwrap();
        assert!(!standings(&other.join("data"), Some(KEY), &root(&other), &pack_of(&other)).code.is_trusted());
        // A record of another version, or one that is not JSON, trusts nothing.
        std::fs::write(trust_path(&data), json!({"version": 1, "packs": {rk.clone(): {"code": tag_of(KEY, &rk, &listing)}}}).to_string()).unwrap();
        assert!(!standings(&data, Some(KEY), &root(&d), &pack).code.is_trusted());
        std::fs::write(trust_path(&data), "not json").unwrap();
        assert!(!standings(&data, Some(KEY), &root(&d), &pack).code.is_trusted());
    }

    #[test]
    fn a_damaged_record_is_not_written_over_so_the_other_decisions_are_not_lost() {
        let d = temp("damaged");
        put(&d, "commands/x.md", "x");
        let data = d.join("data");
        trust(&data, KEY, &root(&d), &pack_of(&d), &[Kind::Text], &no_expect()).unwrap();
        let good = std::fs::read(trust_path(&data)).unwrap();
        std::fs::write(trust_path(&data), &good[..good.len() / 2]).unwrap();
        let e = trust(&data, KEY, &root(&d), &pack_of(&d), &[Kind::Text], &no_expect()).unwrap_err();
        assert!(matches!(&e, TrustError::Record(m) if m.contains("No trust decision was changed")), "{e}");
        assert!(untrust(&data, &root(&d), &[]).is_err());
        assert_eq!(std::fs::read(trust_path(&data)).unwrap(), &good[..good.len() / 2]);
        let names: Vec<String> = std::fs::read_dir(data.join("tools")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["trusted_packs.json".to_string()], "no temporary file is left: {names:?}");
    }

    #[test]
    fn the_yes_is_for_the_files_that_were_shown_and_not_for_ones_that_changed_since() {
        let d = temp("expect");
        put(&d, "hooks.json", r#"{"hooks": []}"#);
        let data = d.join("data");
        let shown = pack_of(&d);
        let expect: BTreeMap<String, String> = [("code".to_string(), shown.digest(Kind::Code))].into();
        // The file changes between the look and the yes.
        put(&d, "hooks.json", r#"{"hooks": [{"event": "PreToolUse", "command": ["x"]}]}"#);
        let e = trust(&data, KEY, &root(&d), &pack_of(&d), &[Kind::Code], &expect).unwrap_err();
        assert!(matches!(e, TrustError::Changed(_)), "{e}");
        assert_eq!(standings(&data, Some(KEY), &root(&d), &pack_of(&d)).code, Standing::NotTrusted);
        trust(&data, KEY, &root(&d), &pack_of(&d), &[Kind::Code], &[("code".to_string(), pack_of(&d).digest(Kind::Code))].into()).unwrap();
        assert!(standings(&data, Some(KEY), &root(&d), &pack_of(&d)).code.is_trusted());
    }

    #[test]
    fn the_view_shows_the_whole_text_of_code_and_only_the_names_of_text() {
        let d = temp("view");
        put(&d, "skills/deploy/SKILL.md", SKILL);
        put(&d, "hooks.json", r#"{"hooks": []}"#);
        put(&d, "policy.json", "{}");
        let pack = pack_of(&d);
        let st = standings(&d.join("data"), Some(KEY), &root(&d), &pack);
        let v = view(&root(&d), &pack, &st);
        assert_eq!(v["kinds"]["code"]["files"][0]["text"], r#"{"hooks": []}"#);
        assert!(v["kinds"]["text"]["files"][0].get("text").is_none(), "{v}");
        assert_eq!((v["kinds"]["text"]["standing"].clone(), v["kinds"]["code"]["standing"].clone()), (json!("not_trusted"), json!("not_trusted")));
        assert_eq!(v["kinds"]["code"]["digest"], json!(pack.digest(Kind::Code)));
        assert!(v["warnings"][0].as_str().unwrap().contains("policy.json is never read"), "{v}");
    }

    #[test]
    fn import_converts_commands_skills_and_agents_and_never_writes_what_runs_programs() {
        let d = temp("import");
        let p = d.join("repo");
        std::fs::create_dir_all(p.join(".claude/commands")).unwrap();
        std::fs::create_dir_all(p.join(".claude/skills/Deploy Site")).unwrap();
        std::fs::create_dir_all(p.join(".claude/agents")).unwrap();
        std::fs::write(p.join(".claude/commands/Review.md"), "review $ARGUMENTS").unwrap();
        std::fs::write(p.join(".claude/commands/notes.txt"), "not a command").unwrap();
        std::fs::write(p.join(".claude/agents/plan.md"), "plan").unwrap();
        std::fs::write(p.join(".claude/skills/Deploy Site/SKILL.md"), "---\nname: Deploy Site\ndescription: Deploy it\n---\nsteps\n").unwrap();
        std::fs::write(p.join(".claude/skills/Deploy Site/checklist.md"), "- one").unwrap();
        std::fs::write(p.join(".claude/settings.json"), r#"{"hooks": {"PreToolUse": [{"command": "evil.exe"}]}, "mcpServers": {"x": {"command": "evil.exe"}}}"#).unwrap();
        std::fs::write(p.join(".mcp.json"), r#"{"mcpServers": {"x": {"command": "evil.exe"}}}"#).unwrap();
        let r = import(&p, Some("claude")).unwrap();
        assert!(r.written.contains(&"commands/review.md".to_string()) && r.written.contains(&"agents/plan.md".to_string()) && r.written.contains(&"skills/deploy-site/SKILL.md".to_string()) && r.written.contains(&"skills/deploy-site/checklist.md".to_string()), "{r:?}");
        assert!(r.skipped.iter().any(|s| s.contains("notes.txt")), "{r:?}");
        assert!(r.manual_review.iter().any(|m| m.contains(".claude/settings.json")) && r.manual_review.iter().any(|m| m.contains(".mcp.json")), "{r:?}");
        // The skill's name was made to match its folder so that it loads.
        let text = std::fs::read_to_string(p.join(".aqua/skills/deploy-site/SKILL.md")).unwrap();
        assert!(text.contains("name: deploy-site") && !text.contains("Deploy Site\ndescription"), "{text}");
        assert!(crate::skills::parse_skill(&text, "deploy-site").is_ok());
        // Nothing that runs a program was written, and nothing is trusted.
        for code in CODE_FILES {
            assert!(!p.join(".aqua").join(code).exists(), "{code} must not be written by an import");
        }
        let pack = scan(&p.join(".aqua"));
        assert!(pack.of_kind(Kind::Code).is_empty());
        assert_eq!(standings(&d.join("data"), Some(KEY), &p, &pack).text, Standing::NotTrusted);
        // A second import writes nothing over what is there.
        std::fs::write(p.join(".aqua/commands/review.md"), "my own").unwrap();
        let again = import(&p, Some("claude")).unwrap();
        assert!(again.written.is_empty() && again.skipped.iter().any(|s| s.contains("there is one already")), "{again:?}");
        assert_eq!(std::fs::read_to_string(p.join(".aqua/commands/review.md")).unwrap(), "my own");
        // Wrong --from, no folder.
        assert!(import(&p, Some("cursor")).unwrap_err().contains("claude or opencode"));
        assert!(import(&d.join("data"), None).unwrap_err().contains("no .claude or .opencode folder"));
    }

    #[test]
    fn import_reads_the_folders_of_opencode_in_both_spellings() {
        let d = temp("import_oc");
        let p = d.join("repo");
        std::fs::create_dir_all(p.join(".opencode/command")).unwrap();
        std::fs::create_dir_all(p.join(".opencode/agents")).unwrap();
        std::fs::create_dir_all(p.join(".opencode/plugin")).unwrap();
        std::fs::write(p.join(".opencode/command/ship.md"), "ship").unwrap();
        std::fs::write(p.join(".opencode/agents/build.md"), "build").unwrap();
        std::fs::write(p.join(".opencode/opencode.json"), r#"{"plugin": ["x"]}"#).unwrap();
        let r = import(&p, None).unwrap();
        assert_eq!(r.written, vec!["commands/ship.md".to_string(), "agents/build.md".to_string()]);
        assert_eq!(r.manual_review.len(), 2, "{r:?}");
    }

    #[test]
    fn the_terminal_trusts_only_when_the_person_says_yes_and_shows_the_code_whole() {
        let d = temp("cli");
        put(&d, "hooks.json", r#"{"hooks": [{"event": "Stop", "command": ["done.exe"]}]}"#);
        put(&d, "commands/x.md", "x");
        let data = d.join("data");
        let folder = root(&d).to_string_lossy().to_string();
        let key = |_create: bool| Some(KEY.to_vec());
        let shown = std::cell::RefCell::new(String::new());
        let no = |s: &str| {
            *shown.borrow_mut() = s.to_string();
            false
        };
        let e = run_cli(&data, &["trust".into(), folder.clone()], &key, &no).unwrap_err();
        assert_eq!(e, "nothing was trusted");
        assert!(shown.borrow().contains("done.exe") && shown.borrow().contains("CODE:") && shown.borrow().contains("TEXT:"), "{}", shown.borrow());
        assert!(!standings(&data, Some(KEY), &root(&d), &pack_of(&d)).code.is_trusted());
        let out = run_cli(&data, &["trust".into(), folder.clone(), "--kinds".into(), "text".into()], &key, &|_| true).unwrap();
        assert!(out.contains("Trusted text"), "{out}");
        let st = standings(&data, Some(KEY), &root(&d), &pack_of(&d));
        assert!(st.text.is_trusted() && !st.code.is_trusted());
        let status = run_cli(&data, &["status".into(), folder.clone()], &key, &no).unwrap();
        assert!(status.contains("text: 1 file(s), trusted") && status.contains("code: 1 file(s), not_trusted"), "{status}");
        assert!(run_cli(&data, &["untrust".into(), folder.clone()], &key, &no).unwrap().contains("Stopped trusting"));
        assert!(run_cli(&data, &["trust".into(), folder.clone()], &|_| None, &|_| true).unwrap_err().contains("no trust key"));
        assert!(run_cli(&data, &["trust".into(), folder.clone(), "--kinds".into(), "other".into()], &key, &|_| true).unwrap_err().contains("--kinds"));
        assert!(run_cli(&data, &[], &key, &no).unwrap_err().contains("usage"));
    }
}
