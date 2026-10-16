//! Keeping an approved draft (card R7-10): one JSON file per draft under `data/recipes/drafts/`.
//!
//! The recipe store of learned recipes (`solutions.db`, `AQUA_RECIPES_V2`) holds tool calls that Aqua replays by itself, so a draft does not go there: a draft is not a recipe until a
//! person has tried it. A draft file is read by nobody but the person; nothing in Aqua loads it, switches it on or runs it. Keeping a draft is undone by deleting the file, and only
//! the file this approval made is deleted (the undo record holds the hash of what was written, and a file that was changed since is left alone).

use super::draft::RecipeDraft;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The most drafts kept at once.
pub const MAX_DRAFTS: usize = 50;

#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn test_dir() -> Option<PathBuf> {
    TEST_DIR.with(|d| d.borrow().clone())
}

#[cfg(not(test))]
fn test_dir() -> Option<PathBuf> {
    None
}

/// Puts the drafts folder of the calling thread back to the real one when it is dropped.
#[cfg(test)]
pub struct TestDirGuard;

#[cfg(test)]
impl Drop for TestDirGuard {
    fn drop(&mut self) {
        TEST_DIR.with(|d| *d.borrow_mut() = None);
    }
}

/// For one test: keep drafts in `dir` (on this thread only, so other tests that run at the same time are not affected).
#[cfg(test)]
pub fn use_dir_for_this_test(dir: PathBuf) -> TestDirGuard {
    TEST_DIR.with(|d| *d.borrow_mut() = Some(dir));
    TestDirGuard
}

/// Where drafts are kept: `data/recipes/drafts`.
pub fn drafts_dir() -> PathBuf {
    test_dir().unwrap_or_else(|| crate::runtime::data_dir().join("recipes").join("drafts"))
}

/// What keeping a draft did, as the proposal records it.
#[derive(Debug, Clone)]
pub struct Kept {
    pub undo: Value,
    pub result: Value,
}

fn fnv64(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn slug(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if out.len() >= max {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "x".to_string()
    } else {
        out
    }
}

/// A draft's id is safe to put in a file name: lower-case letters, digits and hyphens, nothing else.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The ids of the drafts in a folder, sorted.
pub fn list_in(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.push(stem.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// Read a kept draft back (and check it again).
pub fn load_in(dir: &Path, id: &str) -> Result<RecipeDraft, String> {
    if !safe_id(id) {
        return Err("that is not the id of a draft".to_string());
    }
    let text = std::fs::read_to_string(dir.join(format!("{id}.json"))).map_err(|e| format!("the draft could not be read: {e}"))?;
    RecipeDraft::from_json(&text)
}

/// Keep a draft in a folder. The same draft again is the same file and changes nothing. Refused: a draft that breaks a rule of `RecipeDraft::validate`, a different file with the
/// same name, and a folder that already holds `MAX_DRAFTS` drafts.
pub fn keep_in(dir: &Path, draft: &RecipeDraft) -> Result<Kept, String> {
    draft.validate()?;
    let body = draft.to_json();
    let hash = fnv64(body.as_bytes());
    let id = format!("{}-{}-{hash}", slug(&draft.site, 30), slug(&draft.goal, 30));
    let path = dir.join(format!("{id}.json"));
    let existed = match std::fs::read(&path) {
        Ok(bytes) if bytes == body.as_bytes() => true,
        Ok(_) => return Err("a different file with the same name is already in the drafts folder, so it was not replaced".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("the drafts folder could not be read: {e}")),
    };
    if !existed {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not make the drafts folder: {e}"))?;
        if list_in(dir).len() >= MAX_DRAFTS {
            return Err(format!("{MAX_DRAFTS} drafts are already kept; delete one first"));
        }
        std::fs::write(&path, body.as_bytes()).map_err(|e| format!("could not write the draft: {e}"))?;
    }
    let result = json!({
        "draft_id": id,
        "path": path.to_string_lossy(),
        "active": false,
        "stored_as": "a draft file only; nothing loads it or runs it",
        "steps": draft.steps.len(),
        "already_there": existed,
    });
    let undo = if existed { json!({ "kind": "draft_already_kept", "id": id }) } else { json!({ "kind": "delete_recipe_draft", "id": id, "hash": hash }) };
    Ok(Kept { undo, result })
}

/// Delete the file a proposal made, if it is still what was written.
pub fn delete_in(dir: &Path, id: &str, hash: &str) -> Result<String, String> {
    if !safe_id(id) {
        return Err("the undo record names a file that is not a draft".to_string());
    }
    let path = dir.join(format!("{id}.json"));
    match std::fs::read(&path) {
        Ok(bytes) => {
            if fnv64(&bytes) != hash {
                return Err("the draft file was changed since it was kept, so it is not deleted automatically".to_string());
            }
            std::fs::remove_file(&path).map_err(|e| format!("could not delete the draft: {e}"))?;
            Ok(format!("Deleted the draft {id}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(format!("The draft {id} was already gone")),
        Err(e) => Err(format!("the draft could not be read: {e}")),
    }
}

/// What approving a recipe-draft proposal does: read the draft from the proposal's JSON, check it, keep it as a file. It never runs it and never switches it on.
pub fn keep_draft(draft_json: &str) -> Result<Kept, String> {
    let draft = RecipeDraft::from_json(draft_json)?;
    keep_in(&drafts_dir(), &draft)
}

/// What undoing it does, from the record `keep_draft` made.
pub fn undo_kept(undo: &Value) -> Result<String, String> {
    match undo["kind"].as_str() {
        Some("draft_already_kept") => Ok("Nothing to undo: the same draft was already kept before this proposal.".to_string()),
        Some("delete_recipe_draft") => {
            let (Some(id), Some(hash)) = (undo["id"].as_str(), undo["hash"].as_str()) else { return Err("the undo record is damaged".to_string()) };
            delete_in(&drafts_dir(), id, hash)
        }
        _ => Err("this change has no undo".to_string()),
    }
}
