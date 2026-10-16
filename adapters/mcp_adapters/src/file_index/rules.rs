//! What is indexed and how names are compared. Pure functions, no I/O, so every rule is unit-tested.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const FOLDER: i64 = 1;
pub const EXE: i64 = 2;
pub const INSTALLER: i64 = 3;
pub const SHORTCUT: i64 = 4;
pub const UNINSTALLER: i64 = 5;
pub const FILE: i64 = 6;

pub fn kind_from_str(k: &str) -> i64 {
    match k {
        "folder" => FOLDER,
        "executable" => EXE,
        "installer" => INSTALLER,
        "shortcut" => SHORTCUT,
        "uninstaller" => UNINSTALLER,
        _ => FILE,
    }
}

pub fn kind_str(k: i64) -> &'static str {
    match k {
        FOLDER => "folder",
        EXE => "executable",
        INSTALLER => "installer",
        SHORTCUT => "shortcut",
        UNINSTALLER => "uninstaller",
        _ => "file",
    }
}

/// The settings the user controls. Stored as JSON in the index database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Scan and keep the index. On by default: the index never leaves this computer and can be turned off and wiped.
    pub enabled: bool,
    /// Index document files (PDF, Word, Excel, PowerPoint, text, e-books...) in the user's own places and on other
    /// drives, so "open Vishal CV.pdf" is found at once. On by default; not in Program Files or Windows.
    pub index_documents: bool,
    /// Also index every other kind of file (photos, music, video, archives...). Folders, programs, installers and
    /// shortcuts are always indexed.
    pub index_files: bool,
    /// Index the other fixed drives (last, and only folders, programs and shortcuts).
    pub other_drives: bool,
    /// Folders the user added (any drive).
    pub extra_roots: Vec<String>,
    /// Folders (and everything below them) never to index.
    pub exclude_paths: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, index_documents: true, index_files: false, other_drives: true, extra_roots: Vec::new(), exclude_paths: Vec::new() }
    }
}

/// Lower case, accents folded, only letters and digits (of any script) kept: "Personal_AI" and "personal ai" are both
/// "personalai". Folding is done here, not by SQLite, so it is the same for stored names and for queries.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if !c.is_alphanumeric() {
            continue;
        }
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' | 'ė' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ž' | 'ź' | 'ż' => out.push('z'),
            'š' | 'ś' | 'ş' => out.push('s'),
            'ł' => out.push('l'),
            'ř' => out.push('r'),
            'ť' => out.push('t'),
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            c => out.push(c),
        }
    }
    out
}

/// The text a name is searched by: files without their extension ("notepad.exe" is "notepad"), folders as they are.
pub fn search_text(name: &str, is_dir: bool) -> String {
    if is_dir {
        return norm(name);
    }
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    norm(stem)
}

/// Folders that are never indexed or entered, at any depth (compared lower case). They change constantly or hold
/// nothing anyone asks an assistant to open.
pub fn excluded_dir(lower: &str) -> bool {
    lower.starts_with('$')
        || lower.starts_with('.')
        || matches!(
            lower,
            "windows" | "windows.old" | "system volume information" | "winsxs" | "windowsapps" | "recovery" | "perflogs"
                | "msocache" | "config.msi" | "appdata" | "programdata" | "node_modules" | "target" | "__pycache__"
                | "site-packages" | "temp" | "tmp" | "cache" | "caches" | "code cache" | "gpucache" | "shadercache"
                | "inetcache" | "drivers" | "driverstore" | "$recycle.bin"
        )
}

/// Folders that hold credentials, keys or private application data. Not stored at all.
pub fn sensitive_dir(lower: &str) -> bool {
    matches!(
        lower,
        ".ssh" | ".gnupg" | ".aws" | ".azure" | ".kube" | ".docker" | "keepass" | "keepassxc" | "bitwarden" | "1password"
            | "lastpass" | "signal" | "tdata" | "credentials" | "protect" | "crypto" | "user data" | "wallets" | "wallet"
    ) || lower.starts_with("keepass")
}

/// Files that hold credentials or keys. Not stored at all (only matters when ordinary files are indexed).
pub fn sensitive_file(lower: &str, ext: &str) -> bool {
    matches!(
        ext,
        "pem" | "key" | "pfx" | "p12" | "kdbx" | "kdb" | "ppk" | "keystore" | "jks" | "ovpn" | "rdp" | "tfvars"
    ) || lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".env")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || matches!(
            lower,
            ".npmrc" | ".netrc" | ".git-credentials" | ".pypirc" | ".pgpass" | "known_hosts" | "wallet.dat" | "login data" | "cookies"
        )
        || lower.contains("credential")
        || lower.contains("secret")
        || lower.contains("password")
}

/// File types that count as documents: what a person means by "my CV", "the report", "that spreadsheet".
pub const DOCUMENT_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "docm", "dot", "dotx", "rtf", "odt", "txt", "md", "tex", "pages", "xls", "xlsx", "xlsm", "xlsb", "csv", "tsv", "ods",
    "numbers", "ppt", "pptx", "pptm", "pps", "ppsx", "odp", "key", "epub", "mobi", "azw3", "djvu", "xps", "one", "vsd", "vsdx", "pub", "wps",
    "html", "htm", "json", "xml", "log", "ini", "cfg", "yaml", "yml", "ipynb",
];

pub fn is_document_ext(ext: &str) -> bool {
    DOCUMENT_EXTS.contains(&ext)
}

/// "Vishal CV.pdf" -> "Vishal CV": the stored search text is the name without its extension, so a query that ends in a
/// document extension is searched without it. Anything else is left alone ("v1.2" is not an extension).
pub fn strip_document_ext(query: &str) -> String {
    let q = query.trim();
    if let Some((stem, ext)) = q.rsplit_once('.') {
        if !stem.trim().is_empty() && is_document_ext(&ext.trim().to_lowercase()) {
            return stem.trim().to_string();
        }
    }
    q.to_string()
}

/// Download and editor temporaries: they change and vanish, and the final rename marks the folder dirty again.
pub fn temp_file(lower: &str, ext: &str) -> bool {
    lower.starts_with("~$") || matches!(ext, "tmp" | "crdownload" | "part" | "partial" | "download" | "swp" | "swx" | "temp")
}

/// Does a path (given as lower-case components below a watched or indexed root) pass through a folder that is never
/// indexed? The watcher uses this to drop events before they reach the worker.
pub fn path_in_noise<'a>(components: impl IntoIterator<Item = &'a str>) -> bool {
    components.into_iter().any(|c| {
        let l = c.to_lowercase();
        excluded_dir(&l) || sensitive_dir(&l)
    })
}

pub fn under_excluded_path(path_lower: &str, settings: &Settings) -> bool {
    settings.exclude_paths.iter().any(|e| {
        let e = e.trim().trim_end_matches(['\\', '/']).to_lowercase();
        !e.is_empty() && (path_lower == e || path_lower.starts_with(&format!("{e}\\")))
    })
}

/// Windows attribute bits that mean "skip": hidden and system.
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
/// Cloud placeholder (OneDrive Files On-Demand): opening it downloads it.
pub const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
pub const FLAG_CLOUD: i64 = 1;

pub fn cloud_flag(attrs: u32) -> i64 {
    if attrs & (ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0 { FLAG_CLOUD } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_normalise_the_same_whatever_the_spacing_case_or_accents() {
        assert_eq!(norm("Personal_AI"), "personalai");
        assert_eq!(norm("personal ai"), "personalai");
        assert_eq!(norm("Café Déjà-vu"), "cafedejavu");
        assert_eq!(norm("Straße"), "strasse");
        assert_eq!(norm("!!!"), "");
        assert!(norm("दस्तावेज 2024").ends_with("2024") && norm("दस्तावेज 2024").chars().count() > 4, "other scripts are kept, not dropped");
        assert_eq!(norm("文档 Files"), "文档files");
        assert_eq!(search_text("Notepad.exe", false), "notepad");
        assert_eq!(search_text("v1.2", true), "v12");
    }

    #[test]
    fn noise_and_sensitive_names_are_recognised() {
        for d in ["node_modules", ".git", "appdata", "$recycle.bin", "windows"] {
            assert!(excluded_dir(d), "{d}");
        }
        for d in [".ssh", "keepass", "bitwarden", "user data"] {
            assert!(sensitive_dir(d), "{d}");
        }
        assert!(!excluded_dir("personal_ai") && !sensitive_dir("documents"));
        assert!(sensitive_file("id_rsa", "") && sensitive_file("prod.env", "env") && sensitive_file("my passwords.txt", "txt"));
        assert!(sensitive_file("cert.pem", "pem") && !sensitive_file("notes.txt", "txt"));
        assert!(temp_file("a.crdownload", "crdownload") && temp_file("~$doc.docx", "docx") && !temp_file("a.exe", "exe"));
        assert!(path_in_noise(["Users", "x", "AppData", "Local"]) && !path_in_noise(["Users", "x", "Documents"]));
    }

    #[test]
    fn excluded_paths_match_the_folder_and_everything_below_only() {
        let s = Settings { exclude_paths: vec!["D:\\Private".into()], ..Settings::default() };
        assert!(under_excluded_path("d:\\private", &s) && under_excluded_path("d:\\private\\a\\b", &s));
        assert!(!under_excluded_path("d:\\privateer", &s));
    }

    #[test]
    fn a_query_with_a_document_extension_is_searched_without_it() {
        assert_eq!(strip_document_ext("Vishal CV.pdf"), "Vishal CV");
        assert_eq!(strip_document_ext("budget 2024.XLSX"), "budget 2024");
        assert_eq!(strip_document_ext("notes.v2"), "notes.v2");
        assert_eq!(strip_document_ext("v1.2"), "v1.2");
        assert_eq!(strip_document_ext(".pdf"), ".pdf");
        assert!(is_document_ext("pdf") && is_document_ext("docx") && !is_document_ext("jpg") && !is_document_ext("exe"));
    }

    #[test]
    fn cloud_placeholders_are_flagged_from_the_recall_attributes() {
        assert_eq!(cloud_flag(ATTR_RECALL_ON_DATA_ACCESS), FLAG_CLOUD);
        assert_eq!(cloud_flag(0x20), 0);
    }
}
