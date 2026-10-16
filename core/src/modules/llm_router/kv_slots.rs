//! Saving the state of a slot of the model server and putting it back (card OA9-06, task PR-13 and CX-18 of the OpenCode audit). **Off unless `AQUA_LLAMA_SLOT_SAVE` is on.**
//!
//! After any restart the first round of the agent reads its whole prompt again: 1,394 tokens, 18.6 s in the first round of the speed baseline. The model server can write what a slot has
//! read to a file (`POST /slots/N?action=save`) and read it back (`action=restore`) when it was started with `--slot-save-path`; with `AQUA_LLAMA_SLOT_SAVE=on` the launch gives it a
//! folder of Aqua's own ([`ensure_slot_dir`], `data/cache/kv`).
//!
//! **What happens by itself with the switch on (second round of the card).** The agent's warm-up tells this file the start of the prompt it is about to read ([`note_prefix_to_read`],
//! the hash only is kept) and that the read is in the slot ([`note_prefix_read`]); a prompt that replaces it says so ([`note_slot_replaced`]). Then:
//! * **a save** ([`save_on_stop`]) when the server is stopped on purpose (`llama_server::shutdown`: Aqua stopping, the model unloaded, a model imported or removed) or goes idle (`AQUA_LLAMA_IDLE_SECS`),
//!   and only when a start of the agent's prompt is in the slot (a slot that holds a chat turn, or nothing, is not worth a file). Not when the server died or was stopped because it failed
//!   (it cannot be asked), and not when it is replaced by another model;
//! * **a restore** ([`restore_on_start`]) right after the server answers its health check, and only if the file named for this model file (its name, size and time) and the hash of the prompt
//!   start the warm-up has told exists, is a plain file of the folder after links are followed, and was made by this file's names ([`is_slot_file_name`]).
//!
//! Every failure (the build has no endpoint, the server refuses, the file is damaged or was made by another model) is one line of the log and nothing else: no request waits for it, none fails
//! for it, and the call goes on as if there were no file. A save may take [`SAVE_WITHIN`] when the server is stopped and a restore [`RESTORE_WITHIN`] when it has started, and the server is
//! never asked again.
//!
//! The two functions [`experiment_save_slot`] and [`experiment_restore_slot`] of the first round are the measuring test's: no part of Aqua calls them, and a test holds the source to it. The
//! first measurement of what a restore buys is the ignored test `measure_slot_persistence_on_the_8b_model` of `llama_server.rs`, run by the owner with Aqua closed; **the automatic path has
//! only been run against `aqua_fake_llama`, never against a model.**
//!
//! **A saved file is as private as the conversation.** It holds the tokens that were read and what the model made of them, which is the text in another form. So the folder is
//! under the data folder and nowhere else (a link that leaves it is refused), holds nothing but files with names this file made (`kv_<16 hex>_<16 hex>.bin`; nothing else is read,
//! pruned or deleted), is kept to [`MAX_BYTES`] with the oldest file going first, and [`forget_all`] empties it. The name has a hash of the model file (its name, size and time) and a
//! hash of the prefix, never the text. `aqua-cli slots list` and `aqua-cli slots forget` (`cli_tools/slots.rs`) show and delete the files, with their own copy of the rules for the folder and
//! the name so that `cli_tools` uses no module (no code calls [`forget_all`]). **Not built:** a user-only access list on the folder (it inherits that of `data`), and a check that the file
//! was made by the same build of the server (a server that cannot read a file says so, and nothing is started from it).

use super::server_options;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tracing::{debug, info, warn};

/// The most the folder of saved slots may hold: about 220 MB for 1,500 tokens was the audit's estimate for the 8B model, so this is one file and some room.
pub(crate) const MAX_BYTES: u64 = 300 * 1024 * 1024;

/// The folder of saved slots under the data folder.
pub(crate) fn slot_dir(data: &Path) -> PathBuf {
    data.join("cache").join("kv")
}

/// The folder of saved slots, made if it is not there. It must be inside `data` after links are followed (a link or a junction at `cache` or `kv` that leaves the data folder is refused),
/// and a folder.
pub(crate) fn ensure_slot_dir(data: &Path) -> Result<PathBuf, String> {
    let outside = || "the folder for saved slots is not inside the data folder, so it is not used".to_string();
    let root = std::fs::canonicalize(data).map_err(|_| "the data folder cannot be read, so the folder for saved slots is not made".to_string())?;
    // One level at a time, and each level that is there already is looked at (links followed) before the next is made: a link that leaves the data folder is refused before anything
    // is made on the other side of it.
    let mut at = data.to_path_buf();
    for part in ["cache", "kv"] {
        at = at.join(part);
        if at.exists() {
            match std::fs::canonicalize(&at) {
                Ok(real) if real.starts_with(&root) && real.is_dir() => {}
                _ => return Err(outside()),
            }
        } else {
            std::fs::create_dir(&at).map_err(|e| format!("the folder for saved slots could not be made ({})", e.kind()))?;
        }
    }
    Ok(at)
}

/// Is this a name that this file made (so that it may be written, read, pruned or deleted)? `kv_` and 16 hex digits, `_` and 16 hex digits, `.bin`: nothing else, so a name from outside
/// cannot hold a path.
pub(crate) fn is_slot_file_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("kv_").and_then(|r| r.strip_suffix(".bin")) else { return false };
    let Some((model, prefix)) = rest.split_once('_') else { return false };
    let hex16 = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    hex16(model) && hex16(prefix)
}

fn hash16(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The hash of the start of a prompt that goes into a file name: 16 hex digits, and nothing of the text.
pub(crate) fn prefix_hash(prefix: &str) -> String {
    hash16(&[prefix.as_bytes()])
}

/// The name for the state a model made of the prompt start whose hash is `prefix_hash` (see [`prefix_hash`]): the model file's name, size and time, and the hash.
pub(crate) fn file_name_for_hash(model_file: &str, model_bytes: u64, model_modified_secs: u64, prefix_hash: &str) -> String {
    let model = hash16(&[model_file.as_bytes(), &model_bytes.to_le_bytes(), &model_modified_secs.to_le_bytes()]);
    format!("kv_{model}_{prefix_hash}.bin")
}

/// The name for the state a model made of `prefix`: the model file's name, size and time, and the prefix (hashed, never kept).
pub(crate) fn file_name_for(model_file: &str, model_bytes: u64, model_modified_secs: u64, prefix: &str) -> String {
    file_name_for_hash(model_file, model_bytes, model_modified_secs, &prefix_hash(prefix))
}

/// The name for the prompt start whose hash is `prefix_hash` on the model at `path` (its size and time are read from the file).
pub(crate) fn file_name_for_model_hash(path: &Path, prefix_hash: &str) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|_| "the model file cannot be read".to_string())?;
    let secs = meta.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| "the model file has no name".to_string())?;
    Ok(file_name_for_hash(name, meta.len(), secs, prefix_hash))
}

/// The name for `prefix` on the model at `path` (its size and time are read from the file).
pub(crate) fn file_name_for_model(path: &Path, prefix: &str) -> Result<String, String> {
    file_name_for_model_hash(path, &prefix_hash(prefix))
}

/// The files of the folder that this file made: name, size and time. Links and other folders are left out, and so is every name this file did not make.
fn slot_files(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    read.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            (is_slot_file_name(&name) && meta.is_file()).then(|| (e.path(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
        })
        .collect()
}

/// How many bytes the saved slots take.
#[allow(dead_code)] // for the report of the folder; only the tests ask yet
pub(crate) fn used_bytes(dir: &Path) -> u64 {
    slot_files(dir).iter().map(|f| f.1).sum()
}

/// Keeps the folder within `max_bytes`: the oldest file goes first, and the newest stays even when it alone is larger (a file that was just made is the one being used). Gives how many
/// files were deleted.
pub(crate) fn prune(dir: &Path, max_bytes: u64) -> usize {
    let mut files = slot_files(dir);
    files.sort_by_key(|f| f.2);
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let mut deleted = 0;
    while total > max_bytes && files.len() > 1 {
        let (path, size, _) = files.remove(0);
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            deleted += 1;
        }
    }
    deleted
}

/// Deletes every saved slot (and nothing else in the folder). Gives how many files went. No code calls it: `aqua-cli slots forget` does the same with its own code in `cli_tools/slots.rs`.
#[allow(dead_code)] // `aqua-cli slots forget` has its own deleter (no module edge from `cli_tools`); the tests call this one
pub(crate) fn forget_all(dir: &Path) -> usize {
    slot_files(dir).into_iter().filter(|(path, _, _)| std::fs::remove_file(path).is_ok()).count()
}

/// What the server said about one save or restore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotAnswer {
    /// Tokens saved or restored.
    pub tokens: u64,
    /// Bytes written or read.
    pub bytes: u64,
    /// Milliseconds the server took.
    pub ms: u64,
}

/// What the server's answer to a save or a restore said (`n_saved` or `n_restored`, `n_written` or `n_read`, `timings.save_ms` or `restore_ms`).
pub(crate) fn answer_of(reply: &serde_json::Value, save: bool) -> SlotAnswer {
    let n = |key: &str| reply[key].as_u64().unwrap_or(0);
    let (tokens, bytes, ms_key) = if save { (n("n_saved"), n("n_written"), "save_ms") } else { (n("n_restored"), n("n_read"), "restore_ms") };
    let ms = reply["timings"][ms_key].as_f64().filter(|v| v.is_finite() && *v >= 0.0).map_or(0, |v| v.round() as u64);
    SlotAnswer { tokens, bytes, ms }
}

/// One call of the slot endpoint of a server at `port` on this machine (`save` or `restore`). The file name must be one this file made.
pub(crate) fn call(port: u16, key: &str, slot: u32, file: &str, save: bool) -> Result<SlotAnswer, String> {
    call_within(port, key, slot, file, save, Duration::from_secs(120))
}

/// [`call`], given up on after `within` (the server is not asked again; the caller says so in its log).
pub(crate) fn call_within(port: u16, key: &str, slot: u32, file: &str, save: bool, within: Duration) -> Result<SlotAnswer, String> {
    if !is_slot_file_name(file) {
        return Err("that is not a name this file makes for a saved slot".to_string());
    }
    if slot > 15 {
        return Err("that is not a slot the server has".to_string());
    }
    let action = if save { "save" } else { "restore" };
    let never = std::sync::atomic::AtomicBool::new(false);
    let reply = super::llama_server::post_json(port, key, &format!("/slots/{slot}?action={action}"), &serde_json::json!({ "filename": file }), within, &never)
        .map_err(|e| format!("the server did not {action} the slot: {}", e.chars().take(160).collect::<String>()))?;
    Ok(answer_of(&reply, save))
}

/// The slot the agent's prompt is in: the agent's own when the server has two, the only one when it has one.
fn agent_slot() -> u32 {
    super::llama_server::slot_for(super::llama_server::AGENT_SLOT).unwrap_or(0)
}

/// What the experiment functions need to be true: the switch is on and the running server was started with the folder.
fn ready() -> Result<(u16, String, PathBuf), String> {
    if !crate::flags::is_on(&crate::flags::LLAMA_SLOT_SAVE) {
        return Err("AQUA_LLAMA_SLOT_SAVE is off".to_string());
    }
    if !server_options::last_launch_applied("slot_save") {
        return Err("the model server was not started with --slot-save-path (restart it with AQUA_LLAMA_SLOT_SAVE=on, and a build that has the option)".to_string());
    }
    super::llama_server::endpoint_now().ok_or_else(|| "the model server is not running, or is busy with a request".to_string())
}

/// **EXPERIMENT (card OA9-06). No default path calls this.** Saves the slot of the agent's prompt of the running server under the name that goes with `prefix` (the start of the prompt:
/// the same text must be given to [`experiment_restore_slot`]) and keeps the folder within [`MAX_BYTES`].
pub fn experiment_save_slot(prefix: &str) -> Result<SlotAnswer, String> {
    let (port, key, model) = ready()?;
    let file = file_name_for_model(&model, prefix)?;
    let answer = call(port, &key, agent_slot(), &file, true)?;
    if let Ok(dir) = ensure_slot_dir(&crate::runtime::data_dir()) {
        prune(&dir, MAX_BYTES);
    }
    Ok(answer)
}

/// **EXPERIMENT (card OA9-06). No default path calls this.** Puts the slot of the agent's prompt of the running server back from the file that [`experiment_save_slot`] made for `prefix`
/// on the same model file. A missing file, or one a server cannot read, is an error and changes nothing: the next request simply reads its prompt.
pub fn experiment_restore_slot(prefix: &str) -> Result<SlotAnswer, String> {
    let (port, key, model) = ready()?;
    let file = file_name_for_model(&model, prefix)?;
    if !slot_dir(&crate::runtime::data_dir()).join(&file).is_file() {
        return Err("no slot was saved for this model and this prefix".to_string());
    }
    call(port, &key, agent_slot(), &file, false)
}

// ---- saved when the server stops, put back when it starts (card OA9-06, second round; off unless AQUA_LLAMA_SLOT_SAVE is on) ----------------------------------------------------

/// How long the server may take to write the slot while it is being stopped (about 220 MB for the 8B model: a second or two), and to read it when it has just started. The server is
/// given up on after that, the log says so, and nothing else changes.
pub(crate) const SAVE_WITHIN: Duration = Duration::from_secs(15);
pub(crate) const RESTORE_WITHIN: Duration = Duration::from_secs(10);

/// What is known of the start of the agent's prompt: its hash (never the text) and whether it is in the slot now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Prefix {
    hash: String,
    in_slot: bool,
}

/// The one thing that is remembered between the warm-up and the stop and the next start of the server.
struct Memory {
    prefix: Mutex<Option<Prefix>>,
}

impl Memory {
    const fn new() -> Memory {
        Memory { prefix: Mutex::new(None) }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Option<Prefix>> {
        self.prefix.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The warm-up is about to read this start of the agent's prompt: a restore may look for it, and it is not in the slot yet.
    fn to_read(&self, prefix: &str) {
        *self.held() = Some(Prefix { hash: prefix_hash(prefix), in_slot: false });
    }

    /// The read is done: the start of the prompt is in the slot.
    fn read(&self) {
        if let Some(p) = self.held().as_mut() {
            p.in_slot = true;
        }
    }

    /// Another prompt was read into the slot over it.
    fn replaced(&self) {
        if let Some(p) = self.held().as_mut() {
            p.in_slot = false;
        }
    }

    /// A server has just started: its slot holds the start of the agent's prompt only if a saved slot was put back into it. (A slot a stopped server had is gone with it.)
    fn started(&self, done: &Done) {
        if matches!(done, Done::Restored(_)) {
            self.read();
        } else {
            self.replaced();
        }
    }

    /// The hash of the prompt start that a restore looks for.
    fn wanted(&self) -> Option<String> {
        self.held().as_ref().map(|p| p.hash.clone())
    }

    /// The hash of the prompt start that is in the slot now, when there is one.
    fn kept(&self) -> Option<String> {
        self.held().as_ref().filter(|p| p.in_slot).map(|p| p.hash.clone())
    }
}

static MEMORY: Memory = Memory::new();

fn automatic() -> bool {
    crate::flags::is_on(&crate::flags::LLAMA_SLOT_SAVE)
}

/// The agent's warm-up is about to read `prefix` (the exact start of its prompt). With the switch off this does nothing, not even remember a hash.
pub(crate) fn note_prefix_to_read(prefix: &str) {
    if automatic() {
        MEMORY.to_read(prefix);
    }
}

/// The warm-up's read of that prefix is in the slot.
pub(crate) fn note_prefix_read() {
    if automatic() {
        MEMORY.read();
    }
}

/// A prompt that does not start with the prefix was read into the slot: what the slot holds is no longer worth a file.
pub(crate) fn note_slot_replaced() {
    if automatic() {
        MEMORY.replaced();
    }
}

/// What one save or restore came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Done {
    Saved(SlotAnswer),
    Restored(SlotAnswer),
    /// The server was not asked, and why.
    Skipped(&'static str),
    /// The server was asked, or the file was looked at, and it could not be done: one short sentence.
    Failed(String),
}

/// The file that may be put back: a name this file makes, in the folder of saved slots (inside the data folder after links are followed), a plain file that is not a link and is not
/// empty, and whose real place is inside that folder. Never creates anything.
fn restorable(data: &Path, name: &str) -> Result<PathBuf, &'static str> {
    if !is_slot_file_name(name) {
        return Err("that is not a name this file makes for a saved slot");
    }
    let dir = slot_dir(data);
    let (Ok(root), Ok(real_dir)) = (std::fs::canonicalize(data), std::fs::canonicalize(&dir)) else {
        return Err("there is no folder of saved slots");
    };
    if !real_dir.starts_with(&root) || !real_dir.is_dir() {
        return Err("the folder of saved slots is not inside the data folder");
    }
    let file = dir.join(name);
    let meta = std::fs::symlink_metadata(&file).map_err(|_| "no slot was saved for this model and this prompt start")?;
    if !meta.is_file() || meta.len() == 0 {
        return Err("the saved slot is not a plain file, or it is empty");
    }
    match std::fs::canonicalize(&file) {
        Ok(real) if real.starts_with(&real_dir) => Ok(file),
        _ => Err("the saved slot is not inside the folder of saved slots"),
    }
}

/// Saves `slot` of the server at `port` under the name for the prompt start `kept` (its hash) on `model`, and keeps the folder within `cap` bytes. Without a prompt start in the slot
/// nothing is asked of the server.
#[allow(clippy::too_many_arguments)]
fn save_into(data: &Path, port: u16, key: &str, model: &Path, slot: u32, kept: Option<String>, within: Duration, cap: u64) -> Done {
    let Some(hash) = kept else { return Done::Skipped("no start of the agent's prompt is in the slot") };
    let name = match file_name_for_model_hash(model, &hash) {
        Ok(n) => n,
        Err(e) => return Done::Failed(e),
    };
    let dir = match ensure_slot_dir(data) {
        Ok(d) => d,
        Err(e) => return Done::Failed(e),
    };
    match call_within(port, key, slot, &name, true, within) {
        Ok(answer) => {
            prune(&dir, cap);
            Done::Saved(answer)
        }
        Err(e) => Done::Failed(e),
    }
}

/// Puts back `slot` of the server at `port` from the file for the prompt start `wanted` (its hash) on `model`, when that file is there and is the folder's own.
fn restore_from(data: &Path, port: u16, key: &str, model: &Path, slot: u32, wanted: Option<String>, within: Duration) -> Done {
    let Some(hash) = wanted else { return Done::Skipped("the start of the agent's prompt is not known yet") };
    let name = match file_name_for_model_hash(model, &hash) {
        Ok(n) => n,
        Err(e) => return Done::Failed(e),
    };
    if let Err(why) = restorable(data, &name) {
        return Done::Skipped(why);
    }
    match call_within(port, key, slot, &name, false, within) {
        Ok(answer) => Done::Restored(answer),
        Err(e) => Done::Failed(e),
    }
}

/// One line of the log for what happened, and nothing for the person: a failure is never an error of a request.
fn say(what: &str, done: &Done) {
    match done {
        Done::Saved(a) => info!("llama-server slot saved before the server stopped ({} tokens, {} bytes, {} ms)", a.tokens, a.bytes, a.ms),
        Done::Restored(a) => info!("llama-server slot put back after the server started ({} tokens, {} bytes, {} ms)", a.tokens, a.bytes, a.ms),
        Done::Skipped(why) => debug!("llama-server slot {what} skipped: {why}"),
        Done::Failed(why) => warn!("llama-server slot {what} failed, and the call goes on without it: {why}"),
    }
}

/// The server at `port` is being stopped on purpose or has gone idle: save the slot of the agent's prompt, if the switch is on, the server was started with the folder and a start of the
/// agent's prompt is in the slot. Called with the model server's lock held, so no request can be in the middle of the slot; it never fails and never panics.
pub(crate) fn save_on_stop(port: u16, key: &str, model: &Path) {
    if !automatic() {
        return;
    }
    let done = if server_options::last_launch_applied("slot_save") {
        save_into(&crate::runtime::data_dir(), port, key, model, agent_slot(), MEMORY.kept(), SAVE_WITHIN, MAX_BYTES)
    } else {
        Done::Skipped("the model server was not started with --slot-save-path")
    };
    say("save", &done);
}

/// The server at `port` has just answered its health check: put the slot of the agent's prompt back, if the switch is on, the server was started with the folder and the file for this
/// model and this prompt start is there. A request that comes next finds the start of its prompt in the cache, or reads it as it always did.
pub(crate) fn restore_on_start(port: u16, key: &str, model: &Path) {
    if !automatic() {
        return;
    }
    let done = if server_options::last_launch_applied("slot_save") {
        restore_from(&crate::runtime::data_dir(), port, key, model, agent_slot(), MEMORY.wanted(), RESTORE_WITHIN)
    } else {
        Done::Skipped("the model server was not started with --slot-save-path")
    };
    say("restore", &done);
    MEMORY.started(&done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorld;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn a_name_is_made_from_the_model_and_the_prefix_and_holds_neither() {
        let a = file_name_for("Qwen3-8B-Q4_K_M.gguf", 5_030_000_000, 1_760_000_000, "You are Aqua, a local PC agent.");
        assert!(is_slot_file_name(&a), "{a}");
        assert!(!a.contains("Qwen") && !a.contains("Aqua"), "no text of either: {a}");
        // The same inputs give the same name; each input changes it.
        assert_eq!(a, file_name_for("Qwen3-8B-Q4_K_M.gguf", 5_030_000_000, 1_760_000_000, "You are Aqua, a local PC agent."));
        let others = [
            file_name_for("Qwen3-8B-Q8_0.gguf", 5_030_000_000, 1_760_000_000, "You are Aqua, a local PC agent."),
            file_name_for("Qwen3-8B-Q4_K_M.gguf", 5_030_000_001, 1_760_000_000, "You are Aqua, a local PC agent."),
            file_name_for("Qwen3-8B-Q4_K_M.gguf", 5_030_000_000, 1_760_000_001, "You are Aqua, a local PC agent."),
            file_name_for("Qwen3-8B-Q4_K_M.gguf", 5_030_000_000, 1_760_000_000, "You are Aqua, a local PC agent!"),
        ];
        for o in &others {
            assert_ne!(*o, a);
            assert!(is_slot_file_name(o));
        }
        // What is put together cannot be confused: ("ab", "c") is not ("a", "bc").
        assert_ne!(hash16(&[b"ab", b"c"]), hash16(&[b"a", b"bc"]));
    }

    #[test]
    fn only_names_this_file_made_are_names_of_saved_slots() {
        let good = "kv_0123456789abcdef_fedcba9876543210.bin";
        assert!(is_slot_file_name(good));
        for bad in [
            "",
            "kv_.bin",
            "kv_0123456789abcdef.bin",
            "kv_0123456789abcdef_fedcba987654321.bin",
            "kv_0123456789abcdef_fedcba98765432100.bin",
            "kv_0123456789ABCDEF_fedcba9876543210.bin",
            "kv_0123456789abcdef_fedcba9876543210.bin.exe",
            "..\\kv_0123456789abcdef_fedcba9876543210.bin",
            "../kv_0123456789abcdef_fedcba9876543210.bin",
            "sub/kv_0123456789abcdef_fedcba9876543210.bin",
            "kv_0123456789abcdef_fedcba9876543210.BIN",
            "x kv_0123456789abcdef_fedcba9876543210.bin",
            "kv_0123456789abcdeg_fedcba9876543210.bin",
            "model.gguf",
            "notes.txt",
        ] {
            assert!(!is_slot_file_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_folder_is_under_the_data_folder_and_a_link_out_of_it_is_refused() {
        let world = TempWorld::new("kv_slots_dir");
        let data = world.path("data");
        std::fs::create_dir_all(&data).unwrap();
        let dir = ensure_slot_dir(&data).expect("made");
        assert_eq!(dir, data.join("cache").join("kv"));
        assert!(dir.is_dir());
        assert_eq!(ensure_slot_dir(&data).unwrap(), dir, "asking again is fine");
        // The data folder as a plain file: nothing can be made.
        let not_a_folder = world.write("data_file", "x");
        assert!(ensure_slot_dir(&not_a_folder).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_at_the_cache_folder_that_leaves_the_data_folder_is_refused() {
        let world = TempWorld::new("kv_slots_junction");
        let data = world.path("data");
        let outside = world.path("outside");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // `mklink /J` needs no privilege; the junction is removed with `rmdir`, never with a recursive delete.
        let link = data.join("cache");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J", &link.to_string_lossy(), &outside.to_string_lossy()]).output();
        if !made.map(|o| o.status.success()).unwrap_or(false) {
            eprintln!("skipped: no junction could be made here");
            return;
        }
        let result = ensure_slot_dir(&data);
        let _ = std::process::Command::new("cmd").args(["/C", "rmdir", &link.to_string_lossy()]).output();
        assert!(result.is_err(), "a folder that leaves the data folder is not used: {result:?}");
        assert!(!outside.join("kv").exists(), "nothing was made on the other side of the link");
    }

    /// A folder with files of these sizes and ages (the age is how many seconds before now the file was last written), and one file that is not a saved slot.
    fn folder_with(world: &TempWorld, sizes_and_ages: &[(u64, u64)]) -> (PathBuf, Vec<String>) {
        let dir = world.path("kv");
        std::fs::create_dir_all(&dir).unwrap();
        let mut names = Vec::new();
        for (i, (size, age)) in sizes_and_ages.iter().enumerate() {
            let name = format!("kv_{:016x}_{:016x}.bin", i + 1, 0xabc);
            let path = dir.join(&name);
            std::fs::write(&path, vec![7u8; *size as usize]).unwrap();
            let when = SystemTime::now() - Duration::from_secs(*age);
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
            names.push(name);
        }
        std::fs::write(dir.join("notes.txt"), vec![1u8; 500]).unwrap();
        (dir, names)
    }

    #[test]
    fn the_oldest_file_goes_first_and_only_saved_slots_are_ever_deleted() {
        let world = TempWorld::new("kv_slots_prune");
        // Four files of 100 bytes, from the oldest (3 hours) to the newest (1 minute), and a notes file that is not one of ours.
        let (dir, names) = folder_with(&world, &[(100, 10_800), (100, 7_200), (100, 3_600), (100, 60)]);
        assert_eq!(used_bytes(&dir), 400, "the notes file is not counted");
        assert_eq!(prune(&dir, 1_000), 0, "within the cap nothing goes");
        assert_eq!(prune(&dir, 250), 2);
        let left: Vec<bool> = names.iter().map(|n| dir.join(n).exists()).collect();
        assert_eq!(left, [false, false, true, true], "the two oldest went");
        assert!(dir.join("notes.txt").exists(), "a file that is not a saved slot is never touched");
        // The newest stays even when it alone is over the cap.
        assert_eq!(prune(&dir, 10), 1);
        assert!(dir.join(&names[3]).exists() && !dir.join(&names[2]).exists());
        // Forgetting empties the folder of saved slots and of nothing else.
        let second = TempWorld::new("kv_slots_forget");
        let (dir, _) = folder_with(&second, &[(10, 5), (10, 4)]);
        assert_eq!(forget_all(&dir), 2);
        assert_eq!(used_bytes(&dir), 0);
        assert!(dir.join("notes.txt").exists());
        assert_eq!(forget_all(&dir), 0);
        assert_eq!(forget_all(&dir.join("not_there")), 0, "a folder that is not there is nothing to forget");
    }

    /// A server that answers one request with `reply` and tells the test what it was sent (the request line, the key and the body).
    fn one_shot_server(reply: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let body = reply.to_string();
            conn.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
            String::from_utf8_lossy(&raw).to_string()
        });
        (port, handle)
    }

    #[test]
    fn a_save_and_a_restore_call_the_slot_endpoint_with_the_file_name_and_read_what_the_server_says() {
        let file = "kv_0123456789abcdef_fedcba9876543210.bin";
        let (port, seen) = one_shot_server(r#"{"id_slot":1,"filename":"x","n_saved":1394,"n_written":220000000,"timings":{"save_ms":812.4}}"#);
        let saved = call(port, "the-key", 1, file, true).expect("saved");
        assert_eq!(saved, SlotAnswer { tokens: 1394, bytes: 220_000_000, ms: 812 });
        let request = seen.join().unwrap();
        assert!(request.starts_with("POST /slots/1?action=save HTTP/1.1"), "{request}");
        assert!(request.contains("Authorization: Bearer the-key"), "{request}");
        assert!(request.ends_with(&format!(r#"{{"filename":"{file}"}}"#)), "{request}");

        let (port, seen) = one_shot_server(r#"{"id_slot":0,"filename":"x","n_restored":1394,"n_read":220000000,"timings":{"restore_ms":95.0}}"#);
        let restored = call(port, "k", 0, file, false).expect("restored");
        assert_eq!(restored, SlotAnswer { tokens: 1394, bytes: 220_000_000, ms: 95 });
        assert!(seen.join().unwrap().starts_with("POST /slots/0?action=restore HTTP/1.1"));

        // A reply without the numbers is zeros, not a guess.
        assert_eq!(answer_of(&serde_json::json!({}), true), SlotAnswer::default());
        assert_eq!(answer_of(&serde_json::json!({"timings": {"save_ms": -3.0}}), true).ms, 0);
    }

    #[test]
    fn a_name_that_this_file_did_not_make_and_a_slot_that_is_not_one_never_reach_the_server() {
        // No server listens at port 9: if a request were made the error would be about the connection, and these say why they were refused.
        for bad in ["../../etc/passwd", "model.gguf", "kv_x_y.bin", "", "kv_0123456789abcdef_fedcba9876543210.bin\"}"] {
            let e = call(9, "k", 0, bad, true).unwrap_err();
            assert!(e.contains("not a name this file makes"), "{bad:?}: {e}");
        }
        let e = call(9, "k", 99, "kv_0123456789abcdef_fedcba9876543210.bin", true).unwrap_err();
        assert!(e.contains("not a slot"), "{e}");
    }

    #[test]
    fn a_server_that_refuses_gives_an_error_that_says_so_and_is_short() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(2_000));
            conn.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
        });
        let e = call(port, "k", 0, "kv_0123456789abcdef_fedcba9876543210.bin", false).unwrap_err();
        handle.join().unwrap();
        assert!(e.starts_with("the server did not restore the slot: "), "{e}");
        assert!(e.len() < 260, "{}", e.len());
    }

    #[test]
    fn the_experiment_functions_do_nothing_with_the_switch_off_or_with_no_server_started_for_them() {
        use crate::flags;
        flags::with_overrides(&[("AQUA_LLAMA_SLOT_SAVE", None)], || {
            assert_eq!(experiment_save_slot("prefix").unwrap_err(), "AQUA_LLAMA_SLOT_SAVE is off");
            assert_eq!(experiment_restore_slot("prefix").unwrap_err(), "AQUA_LLAMA_SLOT_SAVE is off");
        });
        flags::with_overrides(&[("AQUA_LLAMA_SLOT_SAVE", Some("on"))], || {
            // On, but the last launch of the server did not carry the folder (no launch happened in this test).
            let e = experiment_save_slot("prefix").unwrap_err();
            assert!(e.contains("was not started with --slot-save-path"), "{e}");
        });
    }

    #[test]
    fn no_part_of_aqua_calls_the_experiment_functions() {
        fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    rs_files(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for top in ["core/src", "core/tests", "adapters", "apps/aqua-cli/src"] {
            rs_files(&root.join(top), &mut files);
        }
        let mut callers = Vec::new();
        for f in &files {
            let text = std::fs::read_to_string(f).unwrap_or_default();
            for name in ["experiment_save_slot", "experiment_restore_slot"] {
                // The measuring test of llama_server.rs and the integration test against the fake model server (which only shows that the call reaches the server) are the callers.
                if text.contains(&format!("{name}(")) && !f.ends_with("kv_slots.rs") && !f.ends_with("llama_server.rs") && !f.ends_with("speed_options_server_test.rs") {
                    callers.push(format!("{} calls {name}", f.display()));
                }
            }
        }
        assert!(callers.is_empty(), "the experiment functions are called by nothing but the measuring test of llama_server.rs and the test against the fake server: {callers:?}");
        // And inside llama_server.rs only a test calls them: the line before each call is inside the module of tests.
        let server = std::fs::read_to_string(root.join("core/src/modules/llm_router/llama_server.rs")).unwrap_or_default().replace("\r\n", "\n");
        let tests_at = server.find("#[cfg(test)]\nmod tests {").expect("the tests module");
        for name in ["experiment_save_slot(", "experiment_restore_slot("] {
            for (at, _) in server.match_indices(name) {
                assert!(at > tests_at, "{name} is called outside the tests of llama_server.rs");
            }
        }
    }

    // ---- the automatic save and restore --------------------------------------------------------------------------------------------------------------------------------------

    /// A server that answers one request with this status and body and tells the test what it was sent.
    fn server_saying(status: u16, reply: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = conn.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            conn.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).unwrap();
            String::from_utf8_lossy(&raw).to_string()
        });
        (port, handle)
    }

    /// A listener that nobody should connect to: after the test's calls it must have been left alone.
    fn silent_listener() -> (u16, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        (listener.local_addr().unwrap().port(), listener)
    }

    fn untouched(listener: &TcpListener) -> bool {
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    }

    /// A data folder with a folder of saved slots, a stand-in model file, and the name a save for `start` would have.
    fn data_with_model(world: &TempWorld, start: &str) -> (PathBuf, PathBuf, PathBuf, String) {
        let data = world.path("data");
        std::fs::create_dir_all(&data).unwrap();
        let model = world.write("models/llm/Qwen3-8B-Q4_K_M.gguf", "not a model");
        let dir = ensure_slot_dir(&data).unwrap();
        let name = file_name_for_model_hash(&model, &prefix_hash(start)).unwrap();
        (data, model, dir, name)
    }

    #[test]
    fn the_name_for_a_hash_of_the_prompt_start_is_the_name_for_the_prompt_start_and_holds_no_text() {
        let start = "You are Aqua, a local PC agent.";
        assert_eq!(file_name_for("Qwen3-8B-Q4_K_M.gguf", 5, 6, start), file_name_for_hash("Qwen3-8B-Q4_K_M.gguf", 5, 6, &prefix_hash(start)));
        assert_eq!(prefix_hash(start).len(), 16);
        let world = TempWorld::new("kv_slots_hash_name");
        let model = world.write("models/m.gguf", "x");
        assert_eq!(file_name_for_model(&model, start).unwrap(), file_name_for_model_hash(&model, &prefix_hash(start)).unwrap());
        assert!(!file_name_for_model(&model, start).unwrap().contains("Aqua"));
    }

    #[test]
    fn only_a_prompt_start_that_is_in_the_slot_is_kept_and_only_its_hash_is_remembered() {
        let memory = Memory::new();
        assert_eq!((memory.wanted(), memory.kept()), (None, None));
        memory.to_read("the start of the prompt, A");
        assert_eq!((memory.wanted(), memory.kept()), (Some(prefix_hash("the start of the prompt, A")), None), "a restore may look for it; a save has nothing in the slot to keep");
        memory.read();
        assert_eq!(memory.kept(), Some(prefix_hash("the start of the prompt, A")));
        memory.replaced();
        assert_eq!((memory.kept(), memory.wanted()), (None, Some(prefix_hash("the start of the prompt, A"))), "a chat turn read over it: not worth a file, and the restore still knows what to look for");
        memory.to_read("the start of the prompt, B");
        memory.read();
        assert_eq!(memory.kept(), Some(prefix_hash("the start of the prompt, B")));
        // A server that starts has an empty slot, unless a saved one was put back into it: what the stopped server held is not in it.
        memory.started(&Done::Skipped("no file"));
        assert_eq!(memory.kept(), None);
        memory.started(&Done::Restored(SlotAnswer::default()));
        assert_eq!(memory.kept(), Some(prefix_hash("the start of the prompt, B")), "the restored slot starts with the prompt start that was asked for");
        memory.started(&Done::Failed("refused".to_string()));
        assert_eq!(memory.kept(), None);
        assert!(!format!("{:?}", memory.held()).contains("start of the prompt"), "no text is kept");
    }

    #[test]
    fn with_the_switch_off_nothing_is_remembered_saved_or_restored_and_the_server_is_not_asked() {
        use crate::flags;
        let (port, listener) = silent_listener();
        flags::with_overrides(&[("AQUA_LLAMA_SLOT_SAVE", None)], || {
            let before = MEMORY.wanted();
            note_prefix_to_read("a start that must not be remembered");
            note_prefix_read();
            note_slot_replaced();
            assert_eq!(MEMORY.wanted(), before, "nothing was remembered");
            save_on_stop(port, "k", Path::new("m.gguf"));
            restore_on_start(port, "k", Path::new("m.gguf"));
        });
        assert!(untouched(&listener), "no request was made to the server");
    }

    #[test]
    fn a_slot_is_saved_under_the_name_of_the_model_and_the_prompt_start_and_the_folder_is_pruned_after() {
        let world = TempWorld::new("kv_slots_save");
        let start = "You are Aqua, a local PC agent.";
        let (data, model, dir, name) = data_with_model(&world, start);
        for (n, age) in [(1u64, 7_200u64), (2, 3_600), (3, 60)] {
            let path = dir.join(format!("kv_{n:016x}_{:016x}.bin", 0xabc));
            std::fs::write(&path, vec![7u8; 100]).unwrap();
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
        }
        let (port, seen) = one_shot_server(r#"{"id_slot":0,"filename":"x","n_saved":1394,"n_written":220000000,"timings":{"save_ms":812.4}}"#);
        let done = save_into(&data, port, "the-key", &model, 0, Some(prefix_hash(start)), Duration::from_secs(5), 150);
        assert_eq!(done, Done::Saved(SlotAnswer { tokens: 1394, bytes: 220_000_000, ms: 812 }));
        let request = seen.join().unwrap();
        assert!(request.starts_with("POST /slots/0?action=save HTTP/1.1") && request.contains("Authorization: Bearer the-key"), "{request}");
        assert!(request.ends_with(&format!(r#"{{"filename":"{name}"}}"#)), "the name the helper made, and nothing else: {request}");
        assert!(!request.contains("Aqua"), "never the text");
        let left: Vec<bool> = (1..=3u64).map(|n| dir.join(format!("kv_{n:016x}_{:016x}.bin", 0xabc)).exists()).collect();
        assert_eq!(left, [false, false, true], "the folder was pruned with the helper's rule once the server had written the file");
    }

    #[test]
    fn nothing_is_asked_of_the_server_without_a_prompt_start_in_the_slot_or_a_file_to_put_back() {
        let world = TempWorld::new("kv_slots_nothing");
        let start = "You are Aqua, a local PC agent.";
        let (data, model, dir, name) = data_with_model(&world, start);
        let (port, listener) = silent_listener();
        assert!(matches!(save_into(&data, port, "k", &model, 0, None, Duration::from_secs(5), MAX_BYTES), Done::Skipped(_)), "no start of a prompt in the slot");
        assert!(matches!(restore_from(&data, port, "k", &model, 0, None, Duration::from_secs(5)), Done::Skipped(_)), "no start of a prompt known");
        assert!(matches!(restore_from(&data, port, "k", &model, 0, Some(prefix_hash(start)), Duration::from_secs(5)), Done::Skipped(_)), "no file");
        // A file for another prompt start, and a file for another model file (its size differs, so its name does): neither is this one's.
        std::fs::write(dir.join(file_name_for_model_hash(&model, &prefix_hash("another start")).unwrap()), "state").unwrap();
        let other_model = world.write("models/llm/Other-8B-Q4_K_M.gguf", "not a model, and longer");
        std::fs::write(dir.join(file_name_for_model_hash(&other_model, &prefix_hash(start)).unwrap()), "state").unwrap();
        assert!(!dir.join(&name).exists());
        assert!(matches!(restore_from(&data, port, "k", &model, 0, Some(prefix_hash(start)), Duration::from_secs(5)), Done::Skipped(_)), "the files there are not for this model and this start");
        assert!(untouched(&listener), "the server was never asked");
    }

    #[test]
    fn a_slot_is_put_back_through_the_endpoint_when_the_file_for_this_model_and_prompt_start_is_there() {
        let world = TempWorld::new("kv_slots_restore");
        let start = "You are Aqua, a local PC agent.";
        let (data, model, dir, name) = data_with_model(&world, start);
        std::fs::write(dir.join(&name), "the state of the slot").unwrap();
        let (port, seen) = one_shot_server(r#"{"id_slot":1,"filename":"x","n_restored":1394,"n_read":220000000,"timings":{"restore_ms":95.0}}"#);
        let done = restore_from(&data, port, "the-key", &model, 1, Some(prefix_hash(start)), Duration::from_secs(5));
        assert_eq!(done, Done::Restored(SlotAnswer { tokens: 1394, bytes: 220_000_000, ms: 95 }));
        let request = seen.join().unwrap();
        assert!(request.starts_with("POST /slots/1?action=restore HTTP/1.1"), "{request}");
        assert!(request.ends_with(&format!(r#"{{"filename":"{name}"}}"#)), "{request}");
    }

    #[test]
    fn a_server_that_refuses_a_save_or_a_restore_is_one_short_failed_sentence_and_the_file_stays() {
        let world = TempWorld::new("kv_slots_refused");
        let start = "You are Aqua, a local PC agent.";
        let (data, model, dir, name) = data_with_model(&world, start);
        std::fs::write(dir.join(&name), "a damaged state").unwrap();
        let body = r#"{"error":{"code":400,"message":"Unable to restore slot, no available space in KV cache or invalid slot save file","type":"invalid_request_error"}}"#;
        let (port, handle) = server_saying(400, body);
        let Done::Failed(why) = restore_from(&data, port, "k", &model, 0, Some(prefix_hash(start)), Duration::from_secs(5)) else { panic!("a refusal is a failure") };
        handle.join().unwrap();
        assert!(why.starts_with("the server did not restore the slot: HTTP 400") && why.len() < 260, "{why}");
        assert!(dir.join(&name).exists(), "the file is not deleted for it");
        let (port, handle) = server_saying(501, r#"{"error":{"code":501,"message":"This server does not support slots action","type":"not_supported_error"}}"#);
        let Done::Failed(why) = save_into(&data, port, "k", &model, 0, Some(prefix_hash(start)), Duration::from_secs(5), MAX_BYTES) else { panic!("a refusal is a failure") };
        handle.join().unwrap();
        assert!(why.starts_with("the server did not save the slot: HTTP 501"), "{why}");
        // A server that is not there at all is the same kind of sentence.
        let gone = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(matches!(save_into(&data, gone, "k", &model, 0, Some(prefix_hash(start)), Duration::from_secs(2), MAX_BYTES), Done::Failed(_)));
    }

    #[test]
    fn a_saved_file_is_put_back_only_when_it_is_ours_a_plain_file_inside_the_folder_and_not_empty() {
        let world = TempWorld::new("kv_slots_restorable");
        let (data, _model, dir, _) = data_with_model(&world, "x");
        let name = "kv_0123456789abcdef_fedcba9876543210.bin";
        assert!(restorable(&data, name).is_err(), "no file");
        std::fs::write(dir.join(name), "state").unwrap();
        assert_eq!(restorable(&data, name).unwrap(), dir.join(name));
        std::fs::write(dir.join(name), "").unwrap();
        assert!(restorable(&data, name).unwrap_err().contains("empty"), "an empty file is not a saved slot");
        std::fs::write(dir.join("notes.txt"), "state").unwrap();
        for bad in ["notes.txt", "../kv_0123456789abcdef_fedcba9876543210.bin", "kv_x_y.bin", ""] {
            assert!(restorable(&data, bad).unwrap_err().contains("not a name this file makes"), "{bad:?}");
        }
        std::fs::remove_file(dir.join(name)).unwrap();
        std::fs::create_dir(dir.join(name)).unwrap();
        assert!(restorable(&data, name).is_err(), "a folder with the name of a file is not one");
        // No folder of saved slots at all: nothing is made for a restore.
        let bare = world.path("bare_data");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(restorable(&bare, name).is_err());
        assert!(!bare.join("cache").exists(), "looking for a file makes nothing");
    }

    #[cfg(windows)]
    #[test]
    fn a_saved_file_behind_a_junction_that_leaves_the_data_folder_is_not_put_back() {
        let world = TempWorld::new("kv_slots_restorable_junction");
        let data = world.path("data");
        let outside = world.path("outside");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(outside.join("kv")).unwrap();
        let name = "kv_0123456789abcdef_fedcba9876543210.bin";
        std::fs::write(outside.join("kv").join(name), "state").unwrap();
        let link = data.join("cache");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J", &link.to_string_lossy(), &outside.to_string_lossy()]).output();
        if !made.map(|o| o.status.success()).unwrap_or(false) {
            eprintln!("skipped: no junction could be made here");
            return;
        }
        let result = restorable(&data, name);
        let _ = std::process::Command::new("cmd").args(["/C", "rmdir", &link.to_string_lossy()]).output();
        assert!(result.unwrap_err().contains("not inside the data folder"), "a file that really lives outside the data folder is never read");
    }
}
