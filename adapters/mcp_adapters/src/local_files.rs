//! Find programs and files on this computer by name, and open one the user picked.
//!
//! `local_find` searches the places apps and games live (Start menu, Desktop, Downloads, Documents, Program Files and
//! the fixed drives) and sorts what it finds into executables, installers, shortcuts and folders. `local_open` opens
//! only something that the last search returned, chosen by number, by kind ("the executable") or by its path, so a
//! model cannot make up a path to launch. Scripts and shells are never opened from here.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_HITS: usize = 30;
const SEARCH_BUDGET: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub kind: &'static str,
    pub bytes: u64,
}

static LAST: Mutex<Vec<Hit>> = Mutex::new(Vec::new());
static LAST_QUERY: Mutex<String> = Mutex::new(String::new());
static LAST_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// How long the results of a search stay there for "open 2" and "open the executable". A search is answered with a question ("which one
/// would you like me to open?") and the answer comes within a minute or two. Hours or days later the same words mean a new request, and
/// a list that was left in memory (it lived until Aqua was closed) would open, or run, something from a search nobody remembers.
const FOLLOW_UP_WINDOW: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Keep the results of a search for a follow-up.
fn remember(hits: Vec<Hit>) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = hits;
    *LAST_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
}

/// The results of the last search while they can still be followed up; nothing once they are too old.
fn recent() -> Vec<Hit> {
    let fresh = LAST_AT.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|at| at.elapsed() <= FOLLOW_UP_WINDOW);
    if fresh {
        LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
    } else {
        Vec::new()
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The words of a request that name the thing, without filler like "my", "game" or "on".
fn query_words(query: &str) -> Vec<String> {
    const FILLER: &[&str] = &[
        "a", "an", "the", "my", "me", "on", "in", "for", "of", "to", "system", "computer", "pc", "laptop", "game", "app",
        "application", "program", "file", "files", "exe", "folder", "installer", "setup", "shortcut", "find", "search",
        "look", "locate", "please", "named", "called",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| !w.is_empty() && !FILLER.contains(&w.as_str()))
        .collect()
}

fn matches(name: &str, words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }
    let n = norm(name);
    words.iter().all(|w| n.contains(&norm(w))) || n.contains(&words.iter().map(|w| norm(w)).collect::<String>())
}

/// What a file is, from its name: executable, installer, shortcut or just a file.
pub fn classify(path: &Path, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder";
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let installer_name = ["setup", "install", "installer"].iter().any(|k| name.contains(k));
    match ext.as_str() {
        "exe" if name.starts_with("unins") || name.contains("uninstall") => "uninstaller",
        "exe" if installer_name => "installer",
        "exe" | "com" => "executable",
        "msi" | "msix" | "appx" | "appxbundle" => "installer",
        "lnk" | "url" => "shortcut",
        _ => "file",
    }
}

fn skip_dir(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    crate::file_index::rules::excluded_dir(&n)
        || crate::file_index::rules::sensitive_dir(&n)
        || n.starts_with('$') || n.starts_with('.') || matches!(
        n.as_str(),
        "windows" | "system volume information" | "node_modules" | "target" | "temp" | "tmp" | "winsxs" | "recovery"
            | "programdata" | "appdata" | "microsoft" | "windowsapps" | "drivers" | "cache" | "__pycache__"
    )
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_dir())
}

/// The places apps and the user's files usually are, without walking whole drives.
fn main_roots() -> Vec<(PathBuf, usize)> {
    roots().into_iter().take_while(|(p, _)| !p.to_string_lossy().eq_ignore_ascii_case(&std::env::var("SystemRoot").unwrap_or_default())).collect()
}

/// A bounded live search for the index to fall back on. `main_places_only`: just the usual places (fast), otherwise
/// the drives too.
pub(crate) fn find_live(query: &str, budget: Duration, main_places_only: bool) -> Vec<Hit> {
    let r = if main_places_only { main_roots() } else { roots() };
    find_in(query, &r, budget)
}

/// Where to look, with how deep: the places apps are listed first, then the fixed drives.
fn roots() -> Vec<(PathBuf, usize)> {
    let mut r: Vec<(PathBuf, usize)> = Vec::new();
    let mut add = |p: Option<PathBuf>, depth: usize| {
        if let Some(p) = p {
            if !r.iter().any(|(q, _)| q == &p) {
                r.push((p, depth));
            }
        }
    };
    let appdata = env_dir("APPDATA");
    add(appdata.as_ref().map(|a| a.join("Microsoft/Windows/Start Menu/Programs")), 5);
    add(env_dir("ProgramData").map(|a| a.join("Microsoft/Windows/Start Menu/Programs")), 5);
    let home = env_dir("USERPROFILE");
    for sub in ["Desktop", "Downloads", "Documents"] {
        add(home.as_ref().map(|h| h.join(sub)), 5);
    }
    add(env_dir("PUBLIC").map(|p| p.join("Desktop")), 3);
    add(env_dir("LOCALAPPDATA").map(|a| a.join("Programs")), 4);
    add(env_dir("ProgramFiles"), 4);
    add(env_dir("ProgramFiles(x86)"), 4);
    // The programs that come with Windows (Notepad, Calculator, Paint): only these folders themselves, not below them.
    let windir = env_dir("SystemRoot");
    add(windir.clone(), 0);
    add(windir.map(|w| w.join("System32")), 0);
    // Games and big programs often live outside Program Files: look at every fixed drive.
    for letter in b'C'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\", letter as char));
        if root.is_dir() {
            add(Some(root), 4);
        }
    }
    r
}

/// Search the given roots for `query`. Pure over its inputs, so it can be tested on a temp folder.
pub fn find_in(query: &str, roots: &[(PathBuf, usize)], budget: Duration) -> Vec<Hit> {
    let words = query_words(query);
    let mut hits: Vec<Hit> = Vec::new();
    if words.is_empty() {
        return hits;
    }
    let deadline = Instant::now() + budget;
    let mut seen_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    'roots: for (root, max_depth) in roots {
        let mut stack: Vec<(PathBuf, usize)> = vec![(root.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            if Instant::now() > deadline || hits.len() >= MAX_HITS * 3 {
                break 'roots;
            }
            if !seen_dirs.insert(dir.clone()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if ft.is_dir() {
                    if skip_dir(&name) {
                        continue;
                    }
                    if matches(&name, &words) && !hits.iter().any(|h| h.path == path) {
                        hits.push(Hit { path: path.clone(), kind: "folder", bytes: 0 });
                    }
                    if depth < *max_depth {
                        stack.push((path, depth + 1));
                    }
                } else if matches(&name, &words) && !hits.iter().any(|h| h.path == path) {
                    let kind = classify(&path, false);
                    let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    hits.push(Hit { path, kind, bytes });
                }
            }
        }
    }
    // Things you can start come first, then the shorter (higher up) paths.
    let rank = |k: &str| match k {
        "executable" => 0,
        "installer" => 1,
        "shortcut" => 2,
        "folder" => 3,
        "uninstaller" => 5,
        _ => 4,
    };
    hits.sort_by_key(|h| (rank(h.kind), h.path.as_os_str().len()));
    hits.truncate(MAX_HITS);
    hits
}

fn size_text(bytes: u64) -> String {
    match bytes {
        0 => String::new(),
        b if b >= 1 << 30 => format!(", {:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!(", {} MB", b >> 20),
        b => format!(", {} KB", (b >> 10).max(1)),
    }
}

fn describe(hits: &[Hit]) -> String {
    hits.iter()
        .enumerate()
        .map(|(i, h)| format!("{}. [{}] {}{}", i + 1, h.kind, h.path.display(), size_text(h.bytes)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `local_find`: search this computer and remember the results for `local_open`.
pub fn find(query: &str) -> String {
    *LAST_QUERY.lock().unwrap_or_else(|e| e.into_inner()) = query.to_string();
    let hits = search_hits(query);
    let text = if hits.is_empty() {
        format!("Nothing on this computer matched \"{query}\" (searched the Start menu, Desktop, Downloads, Documents, Program Files and the drives).")
    } else {
        format!(
            "Found {} match(es) for \"{query}\". Tell the user what was found, by kind, and ask which one to open. Open one with local_open.\n{}",
            hits.len(),
            describe(&hits)
        )
    };
    remember(hits);
    text
}

/// Programs that run commands or change the system: never started from a search result.
const NEVER_OPEN: &[&str] = &[
    "cmd", "powershell", "pwsh", "powershell_ise", "wt", "windowsterminal", "conhost", "bash", "wsl", "mintty",
    "regedit", "mmc", "mshta", "wscript", "cscript", "rundll32", "regsvr32", "msiexec", "schtasks", "sc", "reg",
    "bitsadmin", "certutil", "psexec", "taskmgr",
];
/// File types that run script code or change the registry when opened.
const NEVER_OPEN_EXT: &[&str] = &[
    "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "reg", "scr", "cpl", "msc", "pif", "jar", "dll", "sys",
    // More script hosts and files that start a program when opened (compiled help, Java Web Start, Monad scripts, settings handlers).
    // Shortcuts and installers (.lnk, .url, .msi, .appx, .appref-ms) are not here: whether voice may open those is a pending decision (D-12).
    "psc1", "msh", "msh1", "msh2", "mshxml", "msh1xml", "msh2xml", "wsc", "sct", "ws", "chm", "jnlp", "gadget", "application", "settingcontent-ms",
];

const RESERVED_NAMES: &[&str] = &["con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9"];

/// Path shapes that are never opened from a search or an index: network and device paths, `..`, device names, names
/// Windows treats specially (trailing dot or space), and paths too long for the shell to open reliably.
fn unsafe_path_reason(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    if s.starts_with("\\\\") {
        return Some("network and device paths are not opened from a search".into());
    }
    if s.chars().count() > 259 {
        return Some(format!("the path is {} characters long, which Windows cannot open reliably", s.chars().count()));
    }
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => return Some("the path contains '..'".into()),
            std::path::Component::Normal(n) => {
                let n = n.to_string_lossy();
                let stem = n.split('.').next().unwrap_or("").to_ascii_lowercase();
                if RESERVED_NAMES.contains(&stem.as_str()) {
                    return Some(format!("{n} is a reserved Windows device name"));
                }
                if n.ends_with('.') || n.ends_with(' ') {
                    return Some(format!("{n} ends with a dot or a space, which Windows handles unpredictably"));
                }
            }
            _ => {}
        }
    }
    None
}

fn refuse_reason(path: &Path) -> Option<String> {
    if let Some(why) = unsafe_path_reason(path) {
        return Some(why);
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
    if NEVER_OPEN_EXT.contains(&ext.as_str()) {
        return Some(format!("a .{ext} file runs script or system code, so Aqua does not open it from a search"));
    }
    if matches!(ext.as_str(), "exe" | "com") && NEVER_OPEN.contains(&stem.as_str()) {
        return Some(format!("{stem} runs commands or changes the system, so Aqua does not open it from a search"));
    }
    None
}

/// Which search result `selector` means: a number, a kind ("executable", "installer", ...), or a path from the results.
pub fn pick<'a>(selector: &str, hits: &'a [Hit]) -> Result<&'a Hit, String> {
    let s = selector.trim().trim_matches('"').trim();
    if hits.is_empty() {
        return Err("there is no search result to open: search first with local_find".into());
    }
    let digits = s.trim_start_matches(['#', ' ']).trim_end_matches(['.', ')']);
    if let Ok(n) = digits.parse::<usize>() {
        return hits.get(n.wrapping_sub(1)).ok_or_else(|| format!("there is no result number {n}; the last search found {}", hits.len()));
    }
    let lower = s.to_ascii_lowercase();
    let kind = match lower.as_str() {
        "executable" | "exe" | "game" | "program" | "app" | "application" | "the executable" => Some("executable"),
        "installer" | "setup" | "the installer" => Some("installer"),
        "shortcut" | "the shortcut" | "link" => Some("shortcut"),
        "folder" | "directory" | "the folder" => Some("folder"),
        _ => None,
    };
    if let Some(kind) = kind {
        let of_kind: Vec<&Hit> = hits.iter().filter(|h| h.kind == kind).collect();
        return match of_kind.len() {
            0 => Err(format!("the last search found no {kind}")),
            1 => Ok(of_kind[0]),
            n => Err(format!(
                "there are {n} results of kind {kind}. Which number should I open?\n{}",
                of_kind
                    .iter()
                    .map(|h| format!("{}. {}", hits.iter().position(|x| std::ptr::eq(x, *h)).unwrap_or(0) + 1, h.path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        };
    }
    hits.iter()
        .find(|h| h.path.to_string_lossy().eq_ignore_ascii_case(s))
        .ok_or_else(|| "that is not one of the last search results; use its number, or a kind such as \"executable\"".into())
}

/// `local_open`: open a result of the last search the way double-clicking it would.
pub fn open(selector: &str) -> Result<String, String> {
    let hits = recent();
    let hit = pick(selector, &hits)?;
    if let Some(why) = refuse_reason(&hit.path) {
        return Err(why);
    }
    if !hit.path.exists() {
        return Err(format!("{} no longer exists", hit.path.display()));
    }
    crate::shell_open::open_local_path(&hit.path).map_err(|e| format!("{e:#}"))?;
    Ok(format!("Opened the {} {}.", hit.kind, hit.path.display()))
}

/// What to look for, when the request is "search my system for X" / "find X on my computer": it must say it means
/// this computer (a bare "search for X" is a web search, and is the agent's).
pub fn parse_search_request(goal: &str) -> Option<String> {
    let g = goal.trim().trim_end_matches(['.', '!', '?']).to_ascii_lowercase().replace(',', " ");
    let words: Vec<&str> = g.split_whitespace().collect();
    const PLACES: &[&str] = &["system", "computer", "pc", "laptop", "machine", "drive", "drives", "disk", "disks"];
    const VERBS: &[&str] = &["search", "find", "look", "locate", "check"];
    const LEAD: &[&str] = &["for", "a", "an", "the", "game", "app", "program", "file", "files", "folder"];
    let first = words.iter().take(6).position(|w| VERBS.contains(w))?;
    if !words.iter().any(|w| PLACES.contains(w)) {
        // No place named. It is still a search of this computer when it names a document ("search for Vishal CV.pdf") or
        // says "my ... file/folder/document" ("find my visual CV file"). Anything else ("search for pizza places") is the web's.
        let last = words.last().copied().unwrap_or("");
        let doc_token = last.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && crate::file_index::rules::is_document_ext(ext));
        let kind_word = ["file", "files", "folder", "folders", "document", "documents"].contains(&last) && words.contains(&"my");
        if !(doc_token || kind_word) {
            return None;
        }
        let mut name: Vec<&str> = words[first + 1..].to_vec();
        while name.first().is_some_and(|w| LEAD.contains(w) || *w == "my") {
            name.remove(0);
        }
        if kind_word {
            name.pop();
        }
        let query = name.join(" ");
        return (!query_words(&query).is_empty()).then_some(query);
    }
    let rest = &words[first + 1..];
    // "find X on my computer": the name is what comes before "on my computer".
    let cut = rest.iter().enumerate().find_map(|(i, w)| {
        (["on", "in", "from", "across", "within"].contains(w) && rest[i + 1..].iter().take(3).any(|x| PLACES.contains(x))).then_some(i)
    });
    let mut name: Vec<&str> = match cut {
        Some(i) => rest[..i].to_vec(),
        None => {
            // "search my system for X": the name is what follows the "for" after the place.
            let place = rest.iter().position(|w| PLACES.contains(w))?;
            let for_at = rest[place..].iter().position(|w| *w == "for")?;
            rest[place + for_at + 1..].to_vec()
        }
    };
    while name.first().is_some_and(|w| LEAD.contains(w)) {
        name.remove(0);
    }
    let query = name.join(" ");
    (!query_words(&query).is_empty()).then_some(query)
}

/// Words that make a sentence a job to do, not a question about where something is: "write a hello world in the E:\work folder" names a folder and wants a file made, not a search.
/// (Found on 2026-10-08: that very sentence was answered "I searched your computer and found nothing called u write hello world for in python in ...", because the word "folder"
/// was enough to take it for a location question.)
const ACTION_WORDS: &[&str] = &[
    "write", "writes", "create", "make", "save", "build", "generate", "add", "put", "copy", "move", "delete", "remove", "rename", "edit", "update", "append", "compress", "zip", "unzip",
    "extract", "convert", "install", "download", "upload", "paste",
];

/// "tell me the Need for Speed folder location" / "where is Chrome": a place on this computer, not a web search. It must ask where (or for a location or a path): the word "folder"
/// on its own only says what a job is about, and a sentence that asks for something to be made, changed or moved is the agent's, never a search.
pub fn parse_location_request(goal: &str) -> Option<String> {
    let g = goal.trim().trim_end_matches(['.', '!', '?']).to_ascii_lowercase().replace(',', " ");
    let words: Vec<&str> = g.split_whitespace().collect();
    if words.iter().any(|w| ["open", "launch", "run", "start", "search", "find", "look", "locate"].contains(w)) || words.iter().any(|w| ACTION_WORDS.contains(w)) {
        return None;
    }
    if !words.iter().any(|w| ["where", "wheres", "location", "locations", "path", "located"].contains(w)) {
        return None;
    }
    const DROP: &[&str] = &[
        "tell", "me", "the", "a", "an", "please", "can", "you", "where", "is", "are", "location", "path", "of", "my", "folder", "folders", "file", "files", "directory", "directories", "whats", "what",
    ];
    let name: Vec<&str> = words.into_iter().filter(|w| !DROP.contains(w)).collect();
    let query = name.join(" ");
    (!query_words(&query).is_empty()).then_some(query)
}

/// Which result "open the executable", "run the installer", "open number 2" or "the first one" means.
pub fn parse_open_choice(goal: &str) -> Option<String> {
    let g = goal.trim().trim_end_matches(['.', '!', '?']).to_ascii_lowercase();
    let words: Vec<&str> = g.split(|c: char| !c.is_alphanumeric() && c != '#').filter(|w| !w.is_empty()).collect();
    let ordinal = |w: &str| match w {
        "one" | "first" | "1st" => Some(1),
        "two" | "second" | "2nd" => Some(2),
        "three" | "third" | "3rd" => Some(3),
        "four" | "fourth" | "4th" => Some(4),
        "five" | "fifth" | "5th" => Some(5),
        _ => w.trim_start_matches('#').parse::<usize>().ok(),
    };
    let choice_of = |tail: &[&str]| -> Option<String> {
        let mut rest: Vec<&str> = tail.iter().copied().filter(|w| !["the", "that", "this", "it", "please", "result", "number", "item", "option", "file", "say", "pick", "choose", "select"].contains(w)).collect();
        if rest.len() > 1 {
            rest.retain(|w| *w != "one");
        }
        match rest.as_slice() {
            [w] if ordinal(w).is_some() => Some(ordinal(w)?.to_string()),
            [w] if ["executable", "exe", "installer", "setup", "shortcut", "folder", "directory", "game", "program", "app"].contains(w) => Some((*w).to_string()),
            _ => None,
        }
    };
    if let Some(verb) = words.iter().position(|w| ["open", "run", "launch", "start", "install"].contains(w)) {
        if words[..verb].iter().any(|w| !["please", "can", "could", "would", "you", "aqua", "hey", "ok", "okay", "yes", "go", "ahead", "and", "say"].contains(w)) {
            return None;
        }
        return choice_of(&words[verb + 1..]);
    }
    // After "which one?", the reply is just "1" or "one".
    choice_of(&words)
}

fn spoken_kind(kind: &str, n: usize) -> String {
    let (one, many) = match kind {
        "executable" => ("an executable", "executables"),
        "installer" => ("an installer", "installers"),
        "shortcut" => ("a shortcut", "shortcuts"),
        "folder" => ("a folder", "folders"),
        "uninstaller" => ("an uninstaller", "uninstallers"),
        _ => ("a file", "files"),
    };
    if n == 1 { one.to_string() } else { format!("{n} {many}") }
}

/// The sentence for a search: what was found, by kind, and the question. Short, because it is spoken.
pub fn search_reply(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("I searched your computer and found nothing called {query}.");
    }
    let mut parts: Vec<String> = Vec::new();
    for kind in ["executable", "installer", "shortcut", "folder", "uninstaller", "file"] {
        let n = hits.iter().filter(|h| h.kind == kind).count();
        if n > 0 {
            parts.push(spoken_kind(kind, n));
        }
    }
    let list = match parts.len() {
        1 => parts[0].clone(),
        n => format!("{} and {}", parts[..n - 1].join(", "), parts[n - 1]),
    };
    let names: Vec<String> = hits
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, h)| {
            let name = h.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let place = h.path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");
            format!("{}. {} ({}) in {}", i + 1, name, h.kind, place)
        })
        .collect();
    let more = if hits.len() > 4 { format!(" and {} more", hits.len() - 4) } else { String::new() };
    format!("I found {list} for {query}: {}{more}. Which one would you like me to open?", names.join("; "))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
}

/// The folder a hit lives in. A folder hit is itself; a file hit is its parent.
fn hit_place(h: &Hit) -> PathBuf {
    if h.kind == "folder" {
        h.path.clone()
    } else {
        h.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| h.path.clone())
    }
}

/// The sentence for "where is X": the folders, as full paths.
/// A parent such as Videos, and a child such as Support, are not separate locations once the named folder is known.
pub fn location_reply(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("I searched your computer and found nothing called {query}.");
    }
    let words = query_words(query);
    let mut places: Vec<PathBuf> = Vec::new();
    for h in hits {
        let place = hit_place(h);
        if !places.iter().any(|p| same_path(p, &place)) {
            places.push(place);
        }
    }
    let named: Vec<PathBuf> = places
        .iter()
        .filter(|p| matches(p.file_name().and_then(|n| n.to_str()).unwrap_or(""), &words))
        .cloned()
        .collect();
    let pool: Vec<PathBuf> = if named.is_empty() { places } else { named };
    // Keep the highest folder. A path inside another kept folder is the same place.
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut ordered = pool;
    ordered.sort_by_key(|p| p.components().count());
    for p in ordered {
        if kept.iter().any(|k| p.starts_with(k)) {
            continue;
        }
        kept.retain(|k| !k.starts_with(&p));
        kept.push(p);
        if kept.len() == 3 {
            break;
        }
    }
    let text: Vec<String> = kept.iter().map(|p| p.display().to_string()).collect();
    format!("I found {query} here: {}.", text.join("; "))
}

/// Handle "search my system for X" and "open the executable" without the language model. None: not one of those.
pub fn handle_request(goal: &str) -> Option<String> {
    if let Some(query) = parse_location_request(goal) {
        let hits = search_hits(&query);
        remember(hits.clone());
        return Some(location_reply(&query, &hits));
    }
    if let Some(query) = parse_search_request(goal) {
        let hits = search_hits(&query);
        remember(hits.clone());
        return Some(search_reply(&query, &hits));
    }
    let Some(choice) = parse_open_choice(goal) else {
        // "open the Personal AI folder", "run the Need for Speed installer"
        let (name, want) = parse_open_named(goal)?;
        return Some(open_named(&name, want).unwrap_or_else(|nothing| nothing));
    };
    let had_search = !recent().is_empty();
    if !had_search {
        // "open it" or "open 2" with nothing searched is for the agent; a kind with nothing to pick from is not.
        return choice.parse::<usize>().is_err().then(|| "There is nothing to open yet. Ask me to search your computer for a program first.".to_string());
    }
    Some(match open(&choice) {
        Ok(done) => done,
        Err(why) if why.starts_with("there are ") => format!("I need to know which: {why}"),
        Err(why) => format!("I didn't open it: {why}"),
    })
}

/// The file or program names of the last search while it can still be followed up, in the order they were listed (at most five).
pub fn last_result_names() -> Vec<String> {
    recent()
        .iter()
        .take(5)
        .map(|h| std::path::Path::new(&h.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| h.path.to_string_lossy().to_string()))
        .collect()
}

/// The sentence for the last search (what was found, by kind, and the question). The agent says this itself after
/// `local_find` instead of spending model rounds to word it.
pub fn last_search_reply() -> Option<String> {
    let q = LAST_QUERY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if q.is_empty() {
        return None;
    }
    let hits = recent();
    Some(search_reply(&q, &hits))
}

/// What "search my system for X" finds: from the index when it is on and ready, otherwise a bounded live search.
fn search_hits(query: &str) -> Vec<Hit> {
    use crate::file_index::{global, LookupRequest, Want};
    if let Some(ix) = global() {
        let r = ix.lookup(&LookupRequest { query: query.to_string(), want: Want::Search, limit: MAX_HITS });
        if r.index_ready || !r.found.is_empty() {
            return r.found.into_iter().map(|f| Hit { path: f.path, kind: f.kind, bytes: f.size }).collect();
        }
    }
    find_in(query, &roots(), SEARCH_BUDGET)
}

/// "open the X folder", "open my X file", "run the X installer", "open the X game": a name and what kind of thing.
pub fn parse_open_named(goal: &str) -> Option<(String, crate::file_index::Want)> {
    use crate::file_index::Want;
    let g = goal.trim().trim_end_matches(['.', '!', '?']).to_ascii_lowercase().replace(',', " ");
    let words: Vec<&str> = g.split_whitespace().collect();
    let verb_at = words.iter().take(5).position(|w| ["open", "launch", "start", "run", "show"].contains(w))?;
    if words[..verb_at].iter().any(|w| !["please", "can", "could", "would", "you", "aqua", "hey", "ok", "okay", "go", "ahead", "and"].contains(w)) {
        return None;
    }
    let mut rest: Vec<&str> = words[verb_at + 1..].to_vec();
    while rest.first().is_some_and(|w| ["me", "the", "my", "that", "this", "a", "an", "to"].contains(w)) {
        rest.remove(0);
    }
    while rest.last().is_some_and(|w| ["me", "please", "now", "thanks", "for"].contains(w)) {
        rest.pop();
    }
    let last = *rest.last()?;
    // "open Vishal CV.pdf": the last word carries a document extension, so it names a file.
    if let Some((stem, ext)) = last.rsplit_once('.') {
        if !stem.is_empty() && crate::file_index::rules::is_document_ext(ext) && rest.len() <= 6 {
            let mut name: Vec<&str> = rest[..rest.len() - 1].to_vec();
            name.push(stem);
            if !name.iter().any(|w| ["and", "then", "delete", "remove", "move"].contains(w)) {
                return Some((name.join(" "), crate::file_index::Want::File));
            }
        }
    }
    let want = match last {
        "folder" | "folders" | "directory" => Want::Folder,
        "file" | "document" | "doc" => Want::File,
        "installer" | "setup" => Want::Installer,
        "game" | "program" | "app" | "application" => Want::Program,
        _ => return None,
    };
    rest.pop();
    if rest.is_empty() || rest.len() > 6 || rest.iter().any(|w| ["and", "then", "delete", "remove", "move"].contains(w)) {
        return None;
    }
    Some((rest.join(" "), want))
}

/// Find `query` (from the index, or live) and open it if it is clearly the one; otherwise list the candidates and ask.
/// `Err` carries the sentence for "nothing found". `Ok` is an opened item or a question.
pub fn open_named(query: &str, want: crate::file_index::Want) -> Result<String, String> {
    use crate::file_index::{global, Lookup, LookupRequest};
    if want == crate::file_index::Want::Folder {
        let dir = match query {
            "download" | "downloads" => Some("Downloads"),
            "video" | "videos" => Some("Videos"),
            "document" | "documents" => Some("Documents"),
            "music" => Some("Music"),
            "picture" | "pictures" | "photo" | "photos" => Some("Pictures"),
            "desktop" => Some("Desktop"),
            _ => None,
        };
        if let Some(dir) = dir {
            if let Some(path) = std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(dir)).filter(|p| p.is_dir()) {
                return match crate::shell_open::open_local_path(&path) {
                    Ok(()) => Ok(format!("Opened the folder {dir}.")),
                    Err(e) => Ok(format!("I didn't open it: {e:#}")),
                };
            }
        }
    }
    let req = LookupRequest { query: query.to_string(), want, limit: 5 };
    let mut res: Lookup = match global() {
        Some(ix) => ix.lookup(&req),
        None => Lookup {
            found: find_in(query, &roots(), SEARCH_BUDGET)
                .into_iter()
                .take(5)
                .map(|h| crate::file_index::resolve::Found { id: 0, path: h.path, kind: h.kind, size: h.bytes, cloud: false, score: 50 })
                .collect(),
            source: "live",
            ..Lookup::default()
        },
    };
    // A folder inside the one that matched the name is not a second choice.
    let paths: Vec<PathBuf> = res.found.iter().map(|f| f.path.clone()).collect();
    res.found.retain(|f| !paths.iter().any(|other| other != &f.path && f.path.starts_with(other)));
    if res.found.is_empty() {
        return Err(if res.offline_only {
            format!("{query} is on a drive that is not connected right now.")
        } else if res.index_enabled && !res.index_ready {
            format!("I couldn't find {query} yet. I'm still indexing your computer, so try again in a few minutes.")
        } else {
            format!("I couldn't find {query} on this computer. It may have been moved or deleted.")
        });
    }
    let top = &res.found[0];
    let second = res.found.get(1).map_or(i32::MIN, |f| f.score);
    let named = matches(top.path.file_name().and_then(|n| n.to_str()).unwrap_or(""), &query_words(query));
    // One clear winner opens. A weak match, or two close ones, are a question: a misheard name must not open the wrong thing.
    // One folder left, and its name matches, is that folder: the score can sit under 60 when the name is longer than the query.
    let clear = (top.score >= 60 || (res.found.len() == 1 && named)) && (res.found.len() == 1 || top.score - second >= 25);
    let hits: Vec<Hit> = res.found.iter().map(|f| Hit { path: f.path.clone(), kind: f.kind, bytes: f.size }).collect();
    remember(hits.clone());
    if !clear {
        return Ok(search_reply(query, &hits));
    }
    match open("1") {
        Ok(_) => {
            if let Some(ix) = global() {
                ix.mark_opened(top.id);
            }
            let name = top.path.file_name().and_then(|n| n.to_str()).unwrap_or("it");
            let place = top.path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).map(|p| format!(", in {p}")).unwrap_or_default();
            let moved = if res.moved { "That had moved. " } else { "" };
            let cloud = if top.cloud { " It is stored in the cloud, so it may take a moment to download." } else { "" };
            Ok(format!("{moved}Opened the {} {name}{place}.{cloud}", top.kind))
        }
        Err(why) => Ok(format!("I didn't open it: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_name_with_its_extension_is_opened_as_a_file() {
        use crate::file_index::Want;
        assert_eq!(parse_open_named("open Vishal CV.pdf"), Some(("vishal cv".to_string(), Want::File)));
        assert_eq!(parse_open_named("please open my budget 2024.xlsx"), Some(("budget 2024".to_string(), Want::File)));
        assert_eq!(parse_open_named("open the vishal cv file"), Some(("vishal cv".to_string(), Want::File)));
        assert_eq!(parse_open_named("open notepad.exe"), None, "a program is not a document");
        assert_eq!(parse_open_named("open need for speed folder"), Some(("need for speed".to_string(), Want::Folder)));
        assert_eq!(parse_open_named("open download folder for me"), Some(("download".to_string(), Want::Folder)));
        assert_eq!(parse_open_named("open video folder for me"), Some(("video".to_string(), Want::Folder)));
        assert_eq!(parse_search_request("search my system for Vishal CV.pdf").as_deref(), Some("vishal cv.pdf"));
    }

    #[test]
    fn requests_to_search_this_computer_and_to_open_a_result_are_understood() {
        for (goal, want) in [
            ("Search my system for Need for Speed", "need for speed"),
            ("find Need for Speed on my computer", "need for speed"),
            ("Please search my pc for Chrome.", "chrome"),
            ("can you look for discord on my laptop", "discord"),
            ("find my visual CV file", "visual cv"),
            ("search for Vishal CV.pdf", "vishal cv.pdf"),
            ("look for my budget 2024 document", "budget 2024"),
        ] {
            assert_eq!(parse_search_request(goal).as_deref(), Some(want), "{goal}");
        }
        assert_eq!(parse_location_request("tell me need for speed folder location").as_deref(), Some("need for speed"));
        assert_eq!(parse_location_request("where is chrome").as_deref(), Some("chrome"));
        assert_eq!(parse_location_request("tell me the news"), None);
        assert_eq!(parse_location_request("where is the need for speed folder").as_deref(), Some("need for speed"));
        assert_eq!(parse_location_request("search my system for Need for Speed"), None, "a search keeps its own parser");
        let located = location_reply("need for speed", &[Hit { path: PathBuf::from("C:/Games/Need For Speed/nfs.exe"), kind: "executable", bytes: 0 }]);
        assert!(located.contains("C:/Games/Need For Speed") && !located.contains("nfs.exe"), "{located}");
        let nested = location_reply(
            "need for speed",
            &[
                Hit { path: PathBuf::from("C:/Users/lenovo/Videos/Need-for-Speed-Most-Wanted"), kind: "folder", bytes: 0 },
                Hit { path: PathBuf::from("C:/Users/lenovo/Videos/Need for Speed.mp4"), kind: "file", bytes: 0 },
                Hit { path: PathBuf::from("C:/Users/lenovo/Videos/Need-for-Speed-Most-Wanted/Need for Speed Most Wanted"), kind: "folder", bytes: 0 },
                Hit { path: PathBuf::from("C:/Users/lenovo/Videos/Need-for-Speed-Most-Wanted/Need for Speed Most Wanted/Support/readme.txt"), kind: "file", bytes: 0 },
            ],
        );
        assert!(nested.contains("Need-for-Speed-Most-Wanted") && !nested.contains("Support") && !nested.contains("Most Wanted"), "{nested}");
        assert!(!nested.trim_end_matches('.').ends_with("Videos"), "{nested}");
        for not_a_system_search in ["search for the weather", "find me a pizza place", "what is on my computer", "open notepad", "search for pizza places", "find my files", "search for cats.com"] {
            assert_eq!(parse_search_request(not_a_system_search), None, "{not_a_system_search}");
        }
        for (goal, want) in [
            ("Open the executable", "executable"),
            ("run the installer", "installer"),
            ("please launch the shortcut", "shortcut"),
            ("open number 2", "2"),
            ("open the first one", "1"),
            ("open #3", "3"),
            ("1", "1"),
            ("one", "1"),
            ("say 1", "1"),
            ("number one", "1"),
        ] {
            assert_eq!(parse_open_choice(goal).as_deref(), Some(want), "{goal}");
        }
        for other in ["open notepad", "open the weather app", "run the installer and then delete it", "tell me about the installer"] {
            assert_eq!(parse_open_choice(other), None, "{other}");
        }
    }

    #[test]
    fn the_search_reply_says_what_was_found_by_kind_and_asks() {
        let h = |p: &str, k: &'static str| Hit { path: PathBuf::from(p), kind: k, bytes: 0 };
        let r = search_reply("need for speed", &[h("C:/Games/NFS/nfs.exe", "executable"), h("C:/Downloads/setup.exe", "installer")]);
        assert!(r.contains("an executable and an installer") && r.ends_with("Which one would you like me to open?"), "{r}");
        assert!(search_reply("zzz", &[]).contains("found nothing"));
    }

    fn temp_tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("aqua_local_files_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, file) in [
            ("Games/Need For Speed", "nfs.exe"),
            ("Games/Need For Speed", "NeedForSpeed.exe"),
            ("Games/Need For Speed", "Uninstall Need for Speed.exe"),
            ("Downloads", "NeedForSpeed_setup.exe"),
            ("Start Menu", "Need for Speed.lnk"),
            ("Games/Other", "chess.exe"),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(file), b"x").unwrap();
        }
        root
    }

    #[test]
    fn finds_executable_installer_shortcut_and_folder_for_a_game_name() {
        let root = temp_tree("find");
        let hits = find_in("Need for Speed", &[(root.clone(), 5)], Duration::from_secs(5));
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind).collect();
        assert!(kinds.contains(&"executable") && kinds.contains(&"installer") && kinds.contains(&"shortcut") && kinds.contains(&"folder"), "{kinds:?}");
        assert!(hits.iter().all(|h| !h.path.to_string_lossy().contains("chess")));
        assert_eq!(hits[0].kind, "executable", "things you can start come first");
        assert!(hits.iter().any(|h| h.kind == "uninstaller"), "an uninstaller is not offered as the program");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_name_matches_however_it_is_spaced_and_filler_words_are_ignored() {
        assert!(matches("NeedForSpeed.exe", &query_words("search my system for need for speed")));
        assert!(matches("Need for Speed.lnk", &query_words("Need for Speed game")));
        assert!(!matches("chess.exe", &query_words("need for speed")));
        assert!(query_words("find the game on my computer").is_empty(), "no name, no search");
    }

    #[test]
    fn picks_by_number_kind_or_path_and_asks_when_it_is_not_clear() {
        let h = |p: &str, k: &'static str| Hit { path: PathBuf::from(p), kind: k, bytes: 0 };
        let hits = vec![h("C:/a/nfs.exe", "executable"), h("C:/d/setup.exe", "installer"), h("C:/s/nfs.lnk", "shortcut"), h("C:/b/nfs2.exe", "executable")];
        assert_eq!(pick("2", &hits).unwrap().kind, "installer");
        assert_eq!(pick("the installer", &hits).unwrap().path, PathBuf::from("C:/d/setup.exe"));
        assert_eq!(pick("shortcut", &hits).unwrap().kind, "shortcut");
        assert!(pick("executable", &hits).unwrap_err().contains("Which number"), "two executables: ask");
        assert_eq!(pick("c:/b/NFS2.exe", &hits).unwrap().path, PathBuf::from("C:/b/nfs2.exe"));
        assert!(pick("C:/Windows/System32/cmd.exe", &hits).is_err(), "only what the search returned");
        assert!(pick("9", &hits).is_err());
        assert!(pick("executable", &[]).unwrap_err().contains("search first"));
    }

    /// Found in a review: the results of a search were kept until Aqua was closed, so "open 2" a day later opened (or ran) the second result of a
    /// search nobody remembered.
    #[test]
    fn the_results_of_a_search_can_be_followed_up_for_a_while_and_not_for_ever() {
        let hit = |n: &str| Hit { path: PathBuf::from(format!("C:/aqua_no_such_games/{n}/{n}.exe")), kind: "executable", bytes: 10 };
        remember(vec![hit("Alpha"), hit("Beta")]);
        assert_eq!(recent().len(), 2);
        let said = handle_request("open number 2").expect("a follow-up right after the search is understood");
        assert!(said.contains("Beta"), "{said}");
        // A quarter of an hour passes (the clock is not touched: the time of the search is moved into the past).
        let Some(long_ago) = std::time::Instant::now().checked_sub(FOLLOW_UP_WINDOW + std::time::Duration::from_secs(60)) else { return };
        *LAST_AT.lock().unwrap() = Some(long_ago);
        assert!(recent().is_empty(), "too old to follow up");
        assert_eq!(handle_request("open number 2"), None, "left to the agent, as when nothing was searched at all");
        assert!(open("2").is_err());
        remember(vec![hit("Gamma")]);
        assert_eq!(recent().len(), 1, "a new search is followed up again");
    }

    #[test]
    fn scripts_and_shells_are_never_opened() {
        assert!(refuse_reason(Path::new("C:/x/run.bat")).is_some());
        assert!(refuse_reason(Path::new("C:/x/a.ps1")).is_some());
        assert!(refuse_reason(Path::new("C:/Windows/System32/cmd.exe")).is_some());
        for name in ["help.chm", "app.jnlp", "x.wsc", "x.sct", "a.msh1", "a.psc1", "a.application", "a.gadget", "a.settingcontent-ms", "A.CHM"] {
            assert!(refuse_reason(Path::new(&format!("C:/x/{name}"))).is_some(), "{name}");
        }
        assert!(refuse_reason(Path::new("C:/Games/nfs.exe")).is_none());
        // Installers and shortcuts wait for decision D-12 (may voice open them, and from where).
        assert!(refuse_reason(Path::new("C:/Games/setup.msi")).is_none());
        assert!(refuse_reason(Path::new("C:/Games/Game.lnk")).is_none());
    }

    #[test]
    fn a_job_that_names_a_folder_is_not_taken_for_a_search_or_a_location_question() {
        // Found on 2026-10-08: the first of these was answered with "I searched your computer and found nothing called u write hello world for in python in ...".
        for job in [
            r#"can u write a hello world for me in python in "E:\vaibhav_testing\" folder for me"#,
            r"create a folder called test in E:\work",
            "make a new python file in the downloads folder",
            "save this note in my documents folder",
            r"write a hello world program in python and save it in the E:\vaibhav_testing folder",
            "copy report.pdf to the backup folder",
            r"delete the old folder E:\tmp\x",
            r"rename the folder E:\a to b",
            "put the file in the folder",
        ] {
            assert_eq!(parse_location_request(job), None, "{job}");
            assert_eq!(parse_search_request(job), None, "{job}");
            assert_eq!(handle_request(job), None, "{job}: left to the agent, which can make the file");
        }
        // The questions this was written for still work.
        assert_eq!(parse_location_request("where is chrome").as_deref(), Some("chrome"));
        assert_eq!(parse_location_request("tell me need for speed folder location").as_deref(), Some("need for speed"));
        assert_eq!(parse_location_request("what is the path of the need for speed folder").as_deref(), Some("need for speed"));
    }
}
