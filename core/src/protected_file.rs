//! Aqua's own secrets and permission files, kept from the programs the person approves (card R6-10, tasks T6-10.2 and T6-10.3; decision D-17 of 2026-10-08).
//!
//! The two tokens and the files that decide what an approved program may do (`policy.json`, `full_access.json`, `hooks.json`, the user MCP servers and their trust,
//! `trusted_projects.json`, `promoted_config.json`, `audit_keys.json`, `autonomy.json`) are plain files in `data/`. So is every file that says what a family of tools may reach
//! (`fs.json` with its roots, `term.json`, `term_rules.json`, `http.json` with its allow list and credentials, `repos.json`, `ssh.json`, `db.json`, `docker.json`, `k8s.json`,
//! `iac.json`, `cloud.json`, `forge.json`, `mon.json`), the person's own rules (`permissions.json`) and the record of what each MCP server offered (`mcp_lock.json`; card OA8-04,
//! task SE-10): a program the person approved could widen any of them, and the effect would show at the next start. The file tools refuse the `data` folder, but a program the person
//! approves is not a file tool. Two protections, and what each does not do:
//!
//! 1. **A label** ([`protect`], [`protect_all`]): a mandatory integrity label, Medium with no-read-up and no-write-up. A program of Low integrity (the `isolated` tier of
//!    `term.run`) or in an AppContainer (the MXC tier) can then neither read nor write them; the person's own programs and Aqua are not affected. It does **not** stop an
//!    ordinary approved program, which has Medium integrity: `term.run` without `isolated`, `term.start` and a user MCP server still read the tokens. That is what the MXC
//!    tier (card R6-35) is for. Fail-soft: a label that cannot be set is logged and start-up goes on. A file replaced by renaming another over it loses its label.
//! 2. **A watch** ([`Watch`]): a hash of those files taken at start-up. If one of them changes while a call that is not a read is running, **every call that is not a read asks**
//!    from then on, with the reason in the question, until Aqua is restarted (D-17). Nothing is refused and nothing is changed back: the start-up copy is not restored. This
//!    catches a program of any integrity that rewrote the policy, the hooks or a trust file; it does not stop it, and it does not undo it.
//!
//! How a change is blamed on a call: a window opens when a call that is not a read begins and closes when it ends. A change seen **between** windows is Aqua's own or the
//! person's (the full-access switch, a trust answer, an edit in an editor) and becomes the new baseline. The host tells a hook about a call only when it succeeded, so a call that
//! failed or timed out leaves its window open and the next call that is not a read settles it. Limits, kept in the open: a program that keeps running after its call (a
//! `term.start` job, a user MCP server) and writes later is not blamed; a change the person or Aqua makes in a window by itself looks like the program's (and makes
//! everything ask until Aqua is restarted); two calls at once share one window. A file that cannot be read counts as absent.

use aqua_tool_host::hooks::{is_read_only, CallWatch};
use aqua_tool_host::HookCall;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use tracing::{info, warn};

/// The files, in the places Aqua reads them from.
pub fn protected_files(data_dir: &Path) -> Vec<PathBuf> {
    let tools = data_dir.join("tools");
    let mut files = vec![
        crate::api_gateway::auth::token_path(data_dir),
        data_dir.join(crate::api_gateway::auth::UI_TOKEN_FILE),
        crate::tool_host::policy_path(data_dir),
        crate::full_access::path(data_dir),
        crate::hooks::hooks_path(data_dir),
        aqua_mcp_adapters::user_servers::ServerManager::config_path(&tools),
        aqua_mcp_adapters::user_servers::ServerManager::trust_path(&tools),
        crate::project_instructions::trust_path(data_dir),
        // What the person trusted of a repository's `.aqua` pack (card OA11-01): a program of Low integrity that could write it would trust its own hooks. The tags need the key as well.
        crate::project_pack::trust_path(data_dir),
        crate::promoted::path(data_dir),
        // `audit_key::keys_path` is private; this is the same place.
        tools.join("audit_keys.json"),
        // The autonomy level and the caps of one run (card R7-18): a program the person approved could otherwise raise the caps that bound it.
        crate::autonomy::path(data_dir),
    ];
    // Every file the core reads as a config (`tool_host::config_file_paths`, from the table of `config_files`: the tool families' own files, the hooks, the MCP servers, the person's pattern
    // rules) is one that grants authority, and the list is made from that table, so a config file added there is watched without anyone remembering to list it here (card OA8-04, SE-10).
    for path in crate::tool_host::config_file_paths(data_dir) {
        if !files.contains(&path) {
            files.push(path);
        }
    }
    // Two more that grant authority and are not read through that table: what each MCP server was recorded to offer (a changed record would bless a changed tool), and the file of the
    // remote-model door, which is not built yet (listing a file that is not there is harmless: its appearing is a change like any other).
    // A third: the list of model servers Aqua may send a prompt to (card OA9-04). A program that could add an address to it would choose where the prompts go, so it is labelled like the rest (a literal path:
    // the model layer's own constant would be an edge from this module to `model`).
    for extra in [tools.join("mcp_lock.json"), tools.join("remote_model.json"), data_dir.join("models").join("providers.json")] {
        if !files.contains(&extra) {
            files.push(extra);
        }
    }
    files
}

/// Label one file (see the module). `false` when it could not be done, which is logged and never an error for the caller.
pub fn protect(path: &Path) -> bool {
    match aqua_native_tools::sandbox::label_no_read_up(path) {
        Ok(()) => true,
        Err(e) => {
            warn!("protected file: {e}; carrying on without the label on {}", path.display());
            false
        }
    }
}

/// Label every protected file that exists. How many were labelled.
pub fn protect_all(data_dir: &Path) -> usize {
    protected_files(data_dir).iter().filter(|p| p.exists()).filter(|p| protect(p)).count()
}

/// The most of a file that is hashed (the files are small; a program that made one huge is seen by its length).
const MAX_HASHED: u64 = 4 * 1024 * 1024;

/// `None`: the file is absent or cannot be read.
type Print = Option<[u8; 32]>;

fn print_of(path: &Path) -> Print {
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut bytes = Vec::new();
    file.take(MAX_HASHED).read_to_end(&mut bytes).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(len.to_le_bytes());
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Some(out)
}

struct State {
    baseline: Vec<Print>,
    /// A call that is not a read has begun and not ended.
    window_open: bool,
    tripped: Option<String>,
}

/// The watch of the protected files (see the module).
pub struct Watch {
    files: Vec<PathBuf>,
    state: Mutex<State>,
}

impl Watch {
    /// A watch of these files, with their state now as the baseline.
    pub fn new(files: Vec<PathBuf>) -> Self {
        let baseline = files.iter().map(|p| print_of(p)).collect();
        Self { files, state: Mutex::new(State { baseline, window_open: false, tripped: None }) }
    }

    /// What start-up does: label the files that exist, then watch them.
    pub fn start(data_dir: &Path) -> Self {
        let labelled = protect_all(data_dir);
        let watch = Self::new(protected_files(data_dir));
        info!("protected files: {labelled} labelled against programs of Low integrity; {} watched for changes made by an approved program", watch.files.len());
        watch
    }

    fn prints(&self) -> Vec<Print> {
        self.files.iter().map(|p| print_of(p)).collect()
    }

    fn changed_names(&self, before: &[Print], now: &[Print]) -> Vec<String> {
        self.files
            .iter()
            .zip(before.iter().zip(now.iter()))
            .filter(|(_, (b, n))| b != n)
            .map(|(p, _)| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string()))
            .collect()
    }

    /// Why every call that is not a read asks, once the watch has seen a change.
    pub fn tripped(&self) -> Option<String> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).tripped.clone()
    }

    fn trip(&self, st: &mut State, names: &[String]) -> String {
        let why = format!("a protected file changed while a program was running ({}): nothing was changed back, and every call that is not a read asks until Aqua is restarted", names.join(", "));
        warn!("{why}");
        st.tripped = Some(why.clone());
        why
    }

    /// Just before a call. A read neither opens a window nor looks at the files. A call that is not a read settles what happened since the last one (a change inside an open
    /// window trips the watch; a change outside one is taken as the new baseline), opens a window, and asks when the watch has tripped.
    pub fn before_call(&self, not_a_read: bool) -> Option<String> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !not_a_read {
            return None;
        }
        if st.tripped.is_none() {
            let now = self.prints();
            let names = self.changed_names(&st.baseline, &now);
            if !names.is_empty() {
                if st.window_open {
                    self.trip(&mut st, &names);
                }
                st.baseline = now;
            }
        }
        st.window_open = true;
        st.tripped.clone()
    }

    /// Just after a call that ran. For a call that is not a read: a change since the window opened trips the watch (the note returned is for this call, once), and the window
    /// closes with the files as they are now as the baseline.
    pub fn after_call(&self, not_a_read: bool) -> Option<String> {
        if !not_a_read {
            return None;
        }
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.prints();
        let names = self.changed_names(&st.baseline, &now);
        let newly = if !names.is_empty() && st.window_open && st.tripped.is_none() { Some(self.trip(&mut st, &names)) } else { None };
        st.baseline = now;
        st.window_open = false;
        newly
    }
}

impl CallWatch for Watch {
    fn before(&self, call: &HookCall) -> Option<String> {
        self.before_call(!is_read_only(call))
    }

    fn after(&self, call: &HookCall) -> Option<String> {
        self.after_call(!is_read_only(call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_tool_host::hooks::{HookAnswer, Hooks, PostAnswer, Watched};
    use std::sync::Arc;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_protected_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn watch_of(dir: &Path) -> (Watch, PathBuf, PathBuf) {
        let (a, b) = (dir.join("policy.json"), dir.join("hooks.json"));
        std::fs::write(&a, "{\"mode\":\"standard\"}").unwrap();
        std::fs::write(&b, "{}").unwrap();
        (Watch::new(vec![a.clone(), b.clone()]), a, b)
    }

    fn call(classes: &[&str]) -> HookCall {
        HookCall { event: "PreToolUse", tool: "t.x".into(), args: serde_json::json!({}), classes: classes.iter().map(|c| c.to_string()).collect(), target: String::new(), decision: "auto", caller: "agent".into(), run_id: String::new(), ..Default::default() }
    }

    #[test]
    fn a_file_changed_while_a_program_ran_trips_the_watch_for_good_and_every_later_call_that_is_not_a_read_asks() {
        let d = temp("trip");
        let (w, a, _) = watch_of(&d);
        assert_eq!(w.before_call(true), None, "nothing has changed");
        std::fs::write(&a, "{\"mode\":\"yolo\"}").unwrap();
        let note = w.after_call(true).expect("the call that was running when the file changed is the one blamed");
        assert!(note.contains("policy.json") && note.contains("until Aqua is restarted"), "{note}");
        assert!(w.tripped().is_some());
        let again = w.before_call(true).expect("the next call that is not a read asks");
        assert_eq!(again, w.tripped().unwrap());
        assert_eq!(w.before_call(false), None, "a read does not ask");
        assert_eq!(w.after_call(true), None, "the note is given once");
        // Putting the file back does not undo it: only a restart does.
        std::fs::write(&a, "{\"mode\":\"standard\"}").unwrap();
        assert!(w.before_call(true).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_change_made_between_two_calls_is_aquas_own_or_the_persons_and_is_not_blamed_on_a_program() {
        let d = temp("between");
        let (w, a, b) = watch_of(&d);
        assert_eq!(w.before_call(true), None);
        assert_eq!(w.after_call(true), None);
        std::fs::write(&a, "{\"mode\":\"safe\"}").unwrap(); // the person switches a mode in the app
        std::fs::remove_file(&b).unwrap();
        assert_eq!(w.before_call(true), None, "the window was closed when the files changed");
        assert_eq!(w.after_call(true), None);
        assert!(w.tripped().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_call_that_failed_leaves_its_window_open_so_the_next_call_that_is_not_a_read_settles_it() {
        let d = temp("failed");
        let (w, a, _) = watch_of(&d);
        assert_eq!(w.before_call(true), None);
        std::fs::write(&a, "{}").unwrap();
        // No `after_call`: the host tells the hook only about a call that succeeded (a timeout, say, ended this one).
        let why = w.before_call(true).expect("the change is found by the next call");
        assert!(why.contains("policy.json"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_that_appears_or_disappears_while_a_program_runs_counts_as_a_change() {
        let d = temp("appear");
        let missing = d.join("mcp.json");
        let kept = d.join("hooks.json");
        std::fs::write(&kept, "{}").unwrap();
        let w = Watch::new(vec![missing.clone(), kept.clone()]);
        w.before_call(true);
        std::fs::write(&missing, "{\"mcpServers\":{}}").unwrap();
        assert!(w.after_call(true).unwrap().contains("mcp.json"));
        let w = Watch::new(vec![kept.clone()]);
        w.before_call(true);
        std::fs::remove_file(&kept).unwrap();
        assert!(w.after_call(true).unwrap().contains("hooks.json"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_read_opens_no_window_and_looks_at_nothing() {
        let d = temp("read");
        let (w, a, _) = watch_of(&d);
        assert_eq!(w.before_call(false), None);
        std::fs::write(&a, "{}").unwrap();
        assert_eq!(w.after_call(false), None);
        assert_eq!(w.before_call(true), None, "no window was open, so the change is not blamed on anyone");
        assert!(w.tripped().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_protected_files_are_the_tokens_and_the_permission_files_in_the_places_aqua_reads_them() {
        let d = PathBuf::from(if cfg!(windows) { "C:\\aqua\\data" } else { "/aqua/data" });
        let files = protected_files(&d);
        let has = |p: PathBuf| files.contains(&p);
        assert!(has(d.join(".aqua_api_token")) && has(d.join(".aqua_ui_token")));
        for name in ["policy.json", "full_access.json", "mcp.json", "mcp_trusted.json", "trusted_projects.json", "trusted_packs.json", "audit_keys.json"] {
            assert!(has(d.join("tools").join(name)), "{name}");
        }
        assert!(has(d.join("hooks.json")) && has(d.join("promoted_config.json")));
        assert!(has(d.join("tools").join("autonomy.json")) && has(crate::autonomy::path(&d)), "the autonomy setting is watched and labelled like the other permission files");
        // The files that say what a family of tools may reach, the person's own rules and the MCP record (card OA8-04, SE-10).
        for name in ["fs.json", "term.json", "term_rules.json", "http.json", "repos.json", "ssh.json", "db.json", "docker.json", "k8s.json", "iac.json", "cloud.json", "forge.json", "mon.json", "permissions.json", "mcp_lock.json", "remote_model.json"] {
            assert!(has(d.join("tools").join(name)), "{name} grants authority and must be watched");
        }
        let mut names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total, "no file is listed twice: {names:?}");
        // The list of the model servers Aqua may send a prompt to (card OA9-04): it says where prompts go, so a program that Aqua runs may not add an address to it.
        assert!(has(d.join("models").join("providers.json")), "providers.json says where a prompt may be sent and must be watched");
        assert_eq!(total, 29, "eleven that were listed before card OA8-04, fourteen config files of the tool families and the person's rules, the MCP record and the remote-model file, (card OA11-01) the record of the project packs the person trusted, and (card OA9-04) the list of model servers");
    }

    /// Card OA8-04, SE-10: the list does not depend on anyone remembering to add a file. Every function of `tool_host.rs` that names the place of a config file under `data/tools` (the
    /// source is read, as other source-reading tests do) is in the list, and so is every file `config_files::status` reads.
    #[test]
    fn every_tools_config_path_helper_is_in_the_protected_list() {
        let d = PathBuf::from(if cfg!(windows) { "C:\\aqua\\data" } else { "/aqua/data" });
        let files = protected_files(&d);
        let source = include_str!("tool_host.rs");
        let lines: Vec<&str> = source.lines().map(str::trim).collect();
        let mut found = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let Some(rest) = line.strip_prefix("pub fn ") else { continue };
            let Some((name, _)) = rest.split_once("(data_dir: &Path)") else { continue };
            if !name.ends_with("_path") || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                continue;
            }
            let body = lines.get(i + 1).copied().unwrap_or("");
            let file = body.strip_prefix("data_dir.join(\"tools\").join(\"").and_then(|r| r.strip_suffix("\")"));
            let Some(file) = file else { continue };
            assert!(files.contains(&d.join("tools").join(file)), "{name} names tools/{file}, which is not in protected_files: a program the person approves could change it unseen");
            found.push(name.to_string());
        }
        // The policy and thirteen families: a change in the source's habits that made this find nothing must not pass as a pass.
        assert!(found.len() >= 14, "found only {found:?}");
        for report in crate::config_files::status(&d) {
            assert!(files.contains(&report.path), "{} is read by the core as a config file and is not watched", report.file);
        }
    }

    /// Card OA8-04, SE-10 (acceptance): a program the person approved that edits one of these files makes every later call that is not a read ask. It is tried for each of the
    /// files that were not watched before.
    #[test]
    fn editing_a_config_file_during_an_approved_call_makes_the_calls_after_it_ask() {
        let d = temp("config_edit");
        let files = protected_files(&d);
        std::fs::create_dir_all(d.join("tools")).unwrap();
        let newly = ["fs.json", "term.json", "term_rules.json", "http.json", "repos.json", "ssh.json", "db.json", "docker.json", "k8s.json", "iac.json", "cloud.json", "forge.json", "mon.json", "permissions.json", "mcp_lock.json", "remote_model.json"];
        for name in newly {
            let path = d.join("tools").join(name);
            assert!(files.contains(&path), "{name}");
            std::fs::write(&path, "{}").unwrap();
        }
        for name in newly {
            let watch = Watch::new(protected_files(&d));
            assert_eq!(watch.before_call(true), None, "{name}: nothing has changed yet");
            // The program that `term.run` started widens the file.
            std::fs::write(d.join("tools").join(name), "{\"widened\": true}").unwrap();
            let note = watch.after_call(true).unwrap_or_else(|| panic!("{name}: the call that was running when the file changed is the one blamed"));
            assert!(note.contains(name) && note.contains("until Aqua is restarted"), "{note}");
            let later = watch.before_call(true).unwrap_or_else(|| panic!("{name}: the next call that is not a read must ask"));
            assert!(later.contains(name), "{later}");
            assert_eq!(watch.before_call(false), None, "a read still goes through");
            std::fs::write(d.join("tools").join(name), "{}").unwrap();
        }
        // Through the host's own chain of hooks: the call that runs the program, then a write.
        let (watch, run, write) = (Arc::new(Watch::new(protected_files(&d))), call(&["EXECUTE"]), call(&["WRITE"]));
        let hooks = Watched::new(None, watch.clone());
        assert_eq!(hooks.pre_tool(&run), HookAnswer::Pass);
        std::fs::write(d.join("tools").join("fs.json"), "{\"roots\": [\"C:/\"]}").unwrap();
        assert!(hooks.post_tool(&run, true, "").note.is_some_and(|n| n.contains("fs.json")));
        assert!(matches!(hooks.pre_tool(&write), HookAnswer::Ask(why) if why.contains("fs.json")), "the write after it asks");
        assert_eq!(hooks.pre_tool(&call(&["READ"])), HookAnswer::Pass);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A change the person makes between two calls (the app's own settings screens, an editor) is not blamed on a program: widening the list of watched files does not make Aqua's own
    /// edits trip it.
    #[test]
    fn a_config_file_the_person_edits_between_calls_is_not_a_trip() {
        let d = temp("config_between");
        std::fs::create_dir_all(d.join("tools")).unwrap();
        std::fs::write(d.join("tools").join("http.json"), "{}").unwrap();
        let watch = Watch::new(protected_files(&d));
        assert_eq!(watch.before_call(true), None);
        assert_eq!(watch.after_call(true), None);
        std::fs::write(d.join("tools").join("http.json"), "{\"allow\": [\"example.com\"]}").unwrap();
        std::fs::write(d.join("tools").join("permissions.json"), "{\"rules\": []}").unwrap();
        assert_eq!(watch.before_call(true), None, "the window was closed when they changed");
        assert!(watch.tripped().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn labelling_covers_the_files_that_exist_skips_the_others_and_is_safe_to_repeat() {
        let d = temp("label");
        std::fs::create_dir_all(d.join("tools")).unwrap();
        std::fs::write(d.join(".aqua_api_token"), "a").unwrap();
        std::fs::write(d.join(".aqua_ui_token"), "b").unwrap();
        std::fs::write(d.join("tools").join("policy.json"), "{}").unwrap();
        assert_eq!(protect_all(&d), 3);
        assert_eq!(protect_all(&d), 3, "again");
        assert_eq!(std::fs::read_to_string(d.join(".aqua_api_token")).unwrap(), "a", "this process can still read what it labelled");
        assert!(!protect(&d.join("not_there.json")), "a label that cannot be set is reported as false and is not an error");
        let _ = std::fs::remove_dir_all(&d);
    }

    struct Fixed(HookAnswer, Option<String>);

    impl Hooks for Fixed {
        fn pre_tool(&self, _call: &HookCall) -> HookAnswer {
            self.0.clone()
        }
        fn post_tool(&self, _call: &HookCall, _ok: bool, _text: &str) -> PostAnswer {
            PostAnswer { redact: vec![], note: self.1.clone() }
        }
    }

    #[test]
    fn the_watch_can_make_a_call_ask_and_never_lets_one_through_that_the_persons_hook_refuses_or_asks_about() {
        let d = temp("hooks");
        let (w, a, _) = watch_of(&d);
        let w = Arc::new(w);
        let write = call(&["WRITE"]);
        // Untripped: the person's hook alone decides.
        let deny = Watched::new(Some(Arc::new(Fixed(HookAnswer::Deny("no".into()), None))), w.clone());
        assert_eq!(deny.pre_tool(&write), HookAnswer::Deny("no".into()));
        let nothing = Watched::new(None, w.clone());
        assert_eq!(nothing.pre_tool(&write), HookAnswer::Pass);
        std::fs::write(&a, "{}").unwrap();
        assert!(nothing.post_tool(&write, true, "").note.unwrap().contains("policy.json"));
        // Tripped: a call that is not a read asks, a read passes, a refusal stays a refusal, and a hook's question is kept beside ours.
        match nothing.pre_tool(&write) {
            HookAnswer::Ask(why) => assert!(why.starts_with("a protected file changed"), "{why}"),
            other => panic!("expected a question, got {other:?}"),
        }
        assert_eq!(nothing.pre_tool(&call(&["READ"])), HookAnswer::Pass);
        assert_eq!(deny.pre_tool(&write), HookAnswer::Deny("no".into()));
        let both = Watched::new(Some(Arc::new(Fixed(HookAnswer::Ask("hook".into()), Some("hook note".into())))), w.clone());
        match both.pre_tool(&write) {
            HookAnswer::Ask(why) => assert!(why.contains("a protected file changed") && why.ends_with("also: hook"), "{why}"),
            other => panic!("expected a question, got {other:?}"),
        }
        assert_eq!(both.post_tool(&write, true, "").note.as_deref(), Some("hook note"), "the watch's note was given once already");
        let _ = std::fs::remove_dir_all(&d);
    }
}
