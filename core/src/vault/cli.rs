//! `aqua-cli vault ...` (card R6-15): the vault could be started only by a web request with the UI token, so nobody could fill it from a terminal.
//!
//! It works on the vault's own file (`data/vault/vault.db`) like `aqua-cli audit-key` works on the data folder, with Aqua's own data protected as it is in the
//! running program. It reads nothing until a folder is added and `index` is asked for.

use super::{norm, IndexReport, Vault};
use std::path::Path;

pub const USAGE: &str = "usage: aqua-cli vault add <folder> | index | status | forget <folder> | forget --all   [--data DIR]";

/// One command on the vault of `data_dir`; what it prints, or the message of a command that did not work.
pub fn run(data_dir: &Path, args: &[String]) -> Result<String, String> {
    let vault = Vault::open(data_dir)?;
    vault.protect(data_dir);
    match args.first().map(String::as_str) {
        Some("add") => {
            let folder = args.get(1).filter(|a| !a.starts_with("--")).ok_or_else(|| USAGE.to_string())?;
            let shown = vault.add_folder(Path::new(folder))?;
            Ok(format!("Added {shown}. Nothing is read until you run: aqua-cli vault index"))
        }
        Some("index") => Ok(report_text(&vault.index(&|| true))),
        Some("status") | None => Ok(status_text(&vault)),
        Some("forget") => match args.get(1).map(String::as_str) {
            Some("--all") => {
                let n = vault.forget_all()?;
                Ok(format!("Forgot {n} document{} and the list of folders.", if n == 1 { "" } else { "s" }))
            }
            Some(folder) if !folder.starts_with("--") => {
                // The folder as it was written when it was added, or as the person writes it now.
                let listed = vault.folders();
                let wanted = norm(Path::new(folder));
                let wanted_canonical = std::fs::canonicalize(folder).ok().map(|c| norm(&c));
                let Some(name) = listed.iter().find(|f| **f == folder || norm(Path::new(f)) == wanted || Some(norm(Path::new(f))) == wanted_canonical) else {
                    return Err(format!("{folder} is not on the list.{}", if listed.is_empty() { " The vault has no folders.".to_string() } else { format!(" The folders are: {}", listed.join("; ")) }));
                };
                let removed = vault.remove_folder(name)?;
                let mut text = format!("Took {name} off and forgot {} document{} read from it.", removed.documents, if removed.documents == 1 { "" } else { "s" });
                if let Some(e) = removed.compact_error {
                    text.push_str(&format!(" The file could not be compacted afterwards ({e}); what was deleted is overwritten in place."));
                }
                Ok(text)
            }
            _ => Err(USAGE.to_string()),
        },
        _ => Err(USAGE.to_string()),
    }
}

fn status_text(vault: &Vault) -> String {
    let s = vault.stats();
    let mut out = format!("{} document{}, {} passage{}{}.", s.docs, if s.docs == 1 { "" } else { "s" }, s.chunks, if s.chunks == 1 { "" } else { "s" }, if s.paused { " (reading is paused)" } else { "" });
    if s.folders.is_empty() {
        out.push_str("\nNo folder is chosen: add one with `aqua-cli vault add <folder>`.");
    } else {
        out.push_str("\nFolders:");
        for f in &s.folders {
            out.push_str(&format!("\n  {f}"));
        }
    }
    out
}

fn report_text(r: &IndexReport) -> String {
    if r.paused {
        return "Reading is paused (see `status`); nothing was read.".to_string();
    }
    let mut out = format!("Read {} new or changed file{}; {} unchanged; {} gone from the folders and forgotten.", r.indexed, if r.indexed == 1 { "" } else { "s" }, r.unchanged, r.removed);
    if r.skipped_secret + r.skipped_other > 0 {
        out.push_str(&format!("\nNot read: {} that look like secrets by name, {} of another kind, links, or too large.", r.skipped_secret, r.skipped_other));
    }
    if r.truncated {
        out.push_str("\nThe run stopped at its limit of files: run `index` again to go on (nothing it did not get to was forgotten).");
    }
    for u in r.unlisted.iter().take(5) {
        out.push_str(&format!("\nCould not list: {u} (what was read from there is kept)."));
    }
    for f in r.failed.iter().take(5) {
        out.push_str(&format!("\nCould not read: {f}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_vault_cli_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_folder_can_be_added_read_looked_at_and_forgotten_from_the_command_line() {
        let base = data("flow");
        let (data_dir, docs) = (base.join("data"), base.join("docs"));
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "the garden project needs soil").unwrap();
        std::fs::write(docs.join("b.txt"), "the garden project needs water").unwrap();
        // Nothing is chosen yet.
        assert!(run(&data_dir, &args(&["status"])).unwrap().contains("No folder is chosen"));
        // Add, and nothing is read until `index`.
        let added = run(&data_dir, &args(&["add", &docs.to_string_lossy()])).unwrap();
        assert!(added.starts_with("Added ") && added.contains("aqua-cli vault index"), "{added}");
        assert!(run(&data_dir, &args(&["status"])).unwrap().starts_with("0 documents"));
        let read = run(&data_dir, &args(&["index"])).unwrap();
        assert!(read.starts_with("Read 2 new or changed files; 0 unchanged; 0 gone"), "{read}");
        let again = run(&data_dir, &args(&["index"])).unwrap();
        assert!(again.starts_with("Read 0 new or changed files; 2 unchanged"), "{again}");
        let status = run(&data_dir, &args(&["status"])).unwrap();
        assert!(status.starts_with("2 documents") && status.contains("Folders:"), "{status}");
        // Forget it by the name the person gives (not only the exact string it was stored under).
        let forgot = run(&data_dir, &args(&["forget", &docs.to_string_lossy().to_uppercase().replace('\\', "/")])).unwrap();
        assert!(forgot.starts_with("Took ") && forgot.contains("forgot 2 documents"), "{forgot}");
        assert!(run(&data_dir, &args(&["status"])).unwrap().starts_with("0 documents"));
        // Everything.
        run(&data_dir, &args(&["add", &docs.to_string_lossy()])).unwrap();
        run(&data_dir, &args(&["index"])).unwrap();
        assert_eq!(run(&data_dir, &args(&["forget", "--all"])).unwrap(), "Forgot 2 documents and the list of folders.");
    }

    #[test]
    fn a_wrong_command_says_how_to_write_it_and_aquas_own_data_cannot_be_added() {
        let base = data("wrong");
        let data_dir = base.join("data");
        std::fs::create_dir_all(data_dir.join("tools")).unwrap();
        for bad in [args(&["add"]), args(&["add", "--data"]), args(&["forget"]), args(&["forget", "--bogus"]), args(&["bogus"])] {
            assert_eq!(run(&data_dir, &bad).unwrap_err(), USAGE, "{bad:?}");
        }
        assert!(run(&data_dir, &args(&["add", &base.join("missing").to_string_lossy()])).is_err(), "a folder that is not there");
        let not_listed = run(&data_dir, &args(&["forget", &base.to_string_lossy()])).unwrap_err();
        assert!(not_listed.contains("is not on the list") && not_listed.contains("The vault has no folders"), "{not_listed}");
        // The program's own data folder is not for the vault, from the command line either.
        let own = run(&data_dir, &args(&["add", &data_dir.to_string_lossy()])).unwrap_err();
        assert!(own.contains("Aqua's own data"), "{own}");
    }
}
