//! Where Aqua's helper programs live (whisper-cli, deep-filter, llama-cli, llama-server, the Kokoro folder).
//!
//! A development build finds them in `vendor/` through a path fixed when it was compiled, which exists only on the
//! machine that built it. An installed copy needs somewhere that travels with it, so these folders are searched
//! first, in order:
//!
//! 1. `AQUA_ENGINES_DIR`
//! 2. `engines\` next to the program (`<exe folder>\engines`)
//! 3. `resources\engines\` next to the program (where the desktop installer puts bundled files)
//! 4. `%LOCALAPPDATA%\Aqua\engines`
//!
//! Layout inside such a folder: `whisper\whisper-cli.exe`, `deep-filter\deep-filter.exe`,
//! `llama\llama-cli.exe` and `llama\llama-server.exe` (with their DLLs beside them), `kokoro\` (the kokoro-tts
//! folder). The per-engine environment variables (`AQUA_WHISPER_CLI_BIN`, ...) still win over all of this.

use std::path::{Path, PathBuf};

/// The folders searched for installed engines, most specific first.
pub fn engine_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = std::env::var_os("AQUA_ENGINES_DIR").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        roots.push(exe_dir.join("engines"));
        roots.push(exe_dir.join("resources").join("engines"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(local).join("Aqua").join("engines"));
    }
    roots
}

/// First `<root>/<rel>` that is a file.
pub fn find_file(roots: &[PathBuf], rel: &str) -> Option<PathBuf> {
    roots.iter().map(|r| r.join(rel)).find(|p| p.is_file())
}

/// First `<root>/<rel>` that is a folder.
pub fn find_dir(roots: &[PathBuf], rel: &str) -> Option<PathBuf> {
    roots.iter().map(|r| r.join(rel)).find(|p| p.is_dir())
}

/// An installed engine program, e.g. `installed_engine("whisper/whisper-cli.exe")`.
pub fn installed_engine(rel: &str) -> Option<PathBuf> {
    find_file(&engine_roots(), rel)
}

/// An installed engine folder, e.g. `installed_engine_dir("kokoro")`.
pub fn installed_engine_dir(rel: &str) -> Option<PathBuf> {
    find_dir(&engine_roots(), rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_root_that_has_the_file_wins_and_a_missing_one_is_none() {
        let base = std::env::temp_dir().join(format!("aqua_engines_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(a.join("whisper")).unwrap();
        std::fs::create_dir_all(b.join("whisper")).unwrap();
        std::fs::create_dir_all(b.join("kokoro")).unwrap();
        std::fs::write(b.join("whisper").join("whisper-cli.exe"), "b").unwrap();
        let roots = vec![a.clone(), b.clone()];
        assert_eq!(find_file(&roots, "whisper/whisper-cli.exe"), Some(b.join("whisper").join("whisper-cli.exe")));
        std::fs::write(a.join("whisper").join("whisper-cli.exe"), "a").unwrap();
        assert_eq!(find_file(&roots, "whisper/whisper-cli.exe"), Some(a.join("whisper").join("whisper-cli.exe")), "earlier root first");
        assert_eq!(find_file(&roots, "llama/llama-cli.exe"), None);
        assert_eq!(find_dir(&roots, "kokoro"), Some(b.join("kokoro")));
        assert_eq!(find_dir(&roots, "nothing"), None);
    }
}
