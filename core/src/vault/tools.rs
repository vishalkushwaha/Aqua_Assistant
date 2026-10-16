//! The agent's tools over the vault: `vault.search(query, limit?)`, `vault.add(title, text, source?)` and `vault.add_folder(path)`.
//!
//! What a search returns is the person's own files and somebody else's text at once, so it comes back as **untrusted** (a document can hold orders) and
//! **private** (it is the person's: a run that holds it may not send things out without asking). `vault.add` keeps a page or a note: a change of the
//! vault, so it asks like any change. `vault.add_folder` lets the vault read a whole folder of the person's: it asks **every time**, in every mode, and the question names the folder
//! (card R6-15: it is how a spoken "index my documents folder" gets the person's yes on the screen).

use super::Vault;
use aqua_tool_host::{Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct VaultProvider(pub Arc<Vault>);

impl ToolProvider for VaultProvider {
    fn id(&self) -> &str {
        "vault"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, description: &str, classes: Vec<PermClass>, schema: Value, read_only: bool| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: description.into(),
            classes,
            default_decision: Decision::Auto,
            input_schema: schema,
            timeout_default_ms: 10_000,
            timeout_max_ms: 20_000,
            annotations: Annotations { read_only, destructive: false, idempotent: read_only, open_world: false },
            untrusted_output: read_only,
        };
        let mut add_folder = m(
            "vault.add_folder",
            "Let the vault read a folder the user names and keep a search index of it. Always asks the user, who sees the folder.",
            vec![PermClass::Write],
            json!({"type": "object", "properties": {"path": {"type": "string", "minLength": 1, "maxLength": 500}}, "required": ["path"]}),
            false,
        );
        add_folder.default_decision = Decision::AlwaysAsk;
        vec![
            m(
                "vault.search",
                "Search the user's chosen folders and saved pages by words. Gives passages with the file, the place in it and a quote.",
                vec![PermClass::Read],
                json!({"type": "object", "properties": {"query": {"type": "string", "minLength": 2, "maxLength": 300}, "limit": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5}}, "required": ["query"]}),
                true,
            ),
            m(
                "vault.add",
                "Keep a page or a note in the vault so it can be searched later: a title, the text, and where it came from.",
                vec![PermClass::Write],
                json!({"type": "object", "properties": {"title": {"type": "string", "minLength": 1, "maxLength": 200}, "text": {"type": "string", "minLength": 1, "maxLength": 200000}, "source": {"type": "string", "maxLength": 300}}, "required": ["title", "text"]}),
                false,
            ),
            add_folder,
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let summary = match tool {
            "vault.add_folder" => format!(
                "Let the vault read the folder {} and keep a search index of it (files that look like secrets, links and large files are not read)",
                args.get("path").and_then(|v| v.as_str()).unwrap_or("").chars().take(200).collect::<String>()
            ),
            "vault.add" => format!("Keep \"{}\" in the vault", args.get("title").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect::<String>()),
            _ => format!("Search the vault for \"{}\"", args.get("query").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect::<String>()),
        };
        let target = if tool == "vault.add_folder" { args.get("path").and_then(|v| v.as_str()).unwrap_or("").chars().take(200).collect() } else { "the vault".to_string() };
        Ok(Classification { classes: vec![], target, summary, target_allowed: tool != "vault.add_folder" })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let vault = self.0.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<ProviderOutput, ToolError> {
                if tool == "vault.add_folder" {
                    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let shown = vault.add_folder(std::path::Path::new(path)).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))?;
                    // Reading goes on in the background, between the person's requests.
                    let started = super::index_in_background(vault.clone(), || crate::queue::foreground_active() == 0);
                    let text = if started { format!("Added {shown} to the vault. It is being read in the background, between your requests.") } else { format!("Added {shown} to the vault. The vault is being read already; this folder is read on the next reading.") };
                    return Ok(ProviderOutput { data: json!({"folder": shown, "indexing": started}), text, ..Default::default() });
                }
                if tool == "vault.add" {
                    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    vault.add_text(&s("title"), &s("source"), &s("text")).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))?;
                    return Ok(ProviderOutput { data: json!({"kept": s("title")}), text: format!("Kept \"{}\" in the vault.", s("title")), ..Default::default() });
                }
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let hits = vault.search(query, limit);
                let stats = vault.stats();
                let text = if hits.is_empty() {
                    if stats.docs == 0 {
                        "The vault is empty: the user has not chosen any folder to read yet.".to_string()
                    } else {
                        format!("Nothing in the vault shares a word with \"{query}\". (A keyword search: try other words from the passage.)")
                    }
                } else {
                    hits.iter().enumerate().map(|(i, h)| format!("[{}] {} — {} ({})\n    \"{}\"", i + 1, h.path, h.loc, h.source, h.quote.replace('\n', " "))).collect::<Vec<_>>().join("\n")
                };
                Ok(ProviderOutput { data: json!({"passages": hits}), text, private: !hits.is_empty(), ..Default::default() })
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the vault task stopped unexpectedly: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_tool_host::*;
    use std::sync::Mutex;

    fn rig(name: &str) -> (ToolHost, Arc<Vault>, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("aqua_vault_tools_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("notes.txt"), "The launch code word is PINEAPPLE-42.").unwrap();
        let vault = Arc::new(Vault::open_at(&base.join("vault.db")).unwrap());
        let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(base.join("audit.jsonl"))));
        host.register(Arc::new(VaultProvider(vault.clone()))).unwrap();
        (host, vault, docs)
    }

    fn call(host: &ToolHost, tool: &str, args: Value, caller: Caller, run: &str) -> ToolResult {
        let mut ctx = RequestContext::new(caller);
        ctx.agent_run_id = run.into();
        host.call_blocking(ToolRequest { tool: tool.into(), arguments: args, context: ctx })
    }

    #[test]
    fn a_search_finds_the_passage_and_comes_back_untrusted_and_private_so_the_run_that_has_it_asks_before_it_sends_anything() {
        let (host, vault, docs) = rig("search");
        let empty = call(&host, "vault.search", json!({"query": "launch code"}), Caller::Agent, "r0");
        assert!(empty.ok && empty.text.contains("vault is empty"), "{}", empty.text);
        vault.add_folder(&docs).unwrap();
        vault.index(&|| true);
        let res = call(&host, "vault.search", json!({"query": "launch code word"}), Caller::Agent, "r1");
        assert!(res.ok, "{}", res.text);
        assert!(res.text.starts_with("<untrusted:") && res.text.contains("PINEAPPLE-42") && res.text.contains("notes.txt"), "{}", res.text);
        assert!(host.is_tainted("r1") && host.is_private("r1"), "the run now holds text from outside and the person's files");
        let none = call(&host, "vault.search", json!({"query": "zebra giraffe"}), Caller::Agent, "r2");
        assert!(none.text.contains("Nothing in the vault shares a word") && !host.is_private("r2"), "{}", none.text);
        assert_eq!(call(&host, "vault.search", json!({"query": "x"}), Caller::Agent, "r3").error.unwrap().code, ErrorCode::InvalidArguments);
        // The voice may search (it only reads).
        assert!(call(&host, "vault.search", json!({"query": "launch code"}), Caller::Voice, "r4").ok);
    }

    /// Card R6-15: how a spoken "index my documents folder" gets the person's yes: the tool asks every time and names the folder, and the voice cannot do it itself.
    #[test]
    fn choosing_a_folder_always_asks_and_names_it_and_a_no_adds_nothing() {
        // This test starts a reading in the background; the vault's other tests that start one must not run beside it (there is one flag for "a reading is going").
        let _one_at_a_time = crate::vault::ONE_READING_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let (host, vault, docs) = rig("addfolder");
        struct Say(Mutex<Vec<(String, bool)>>, ApprovalOutcome);
        impl Approver for Say {
            fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
                self.0.lock().unwrap().push((req.summary, req.always_ask));
                let a = self.1;
                Box::pin(async move { a })
            }
        }
        let path = docs.to_string_lossy().into_owned();
        // With nobody to ask, nothing is added (a clean run: a change to the vault alone would not have asked, a folder of the person's does).
        assert!(!call(&host, "vault.add_folder", json!({"path": path}), Caller::Agent, "r0").ok);
        assert!(vault.folders().is_empty());
        // The question names the folder and is always asked, in a run that has read nothing as in any other; a no adds nothing.
        let no = Arc::new(Say(Mutex::new(vec![]), ApprovalOutcome::Denied));
        host.set_approver(no.clone());
        assert!(!call(&host, "vault.add_folder", json!({"path": path}), Caller::Agent, "r1").ok);
        let asked = no.0.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].0.starts_with("Let the vault read the folder ") && asked[0].0.contains(&path), "{asked:?}");
        assert!(asked[0].1, "always asked");
        assert!(vault.folders().is_empty());
        // A yes, even "for the session", is asked again the next time.
        let yes = Arc::new(Say(Mutex::new(vec![]), ApprovalOutcome::Approved(ApprovalScope::Session)));
        host.set_approver(yes.clone());
        let ok = call(&host, "vault.add_folder", json!({"path": path}), Caller::Agent, "r2");
        assert!(ok.ok && ok.text.contains("Added "), "{}", ok.text);
        assert_eq!(vault.folders().len(), 1);
        assert!(call(&host, "vault.add_folder", json!({"path": path}), Caller::Agent, "r3").ok);
        assert_eq!(yes.0.lock().unwrap().len(), 2, "asked each time");
        // The voice cannot do it itself, and a folder that is not there is refused with the reason.
        assert!(!call(&host, "vault.add_folder", json!({"path": path}), Caller::Voice, "r4").ok);
        let missing = call(&host, "vault.add_folder", json!({"path": format!("{path}_not_there")}), Caller::Agent, "r5");
        assert_eq!(missing.error.unwrap().code, ErrorCode::InvalidArguments);
        // Let the reading that was started end before the next test may start its own.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while crate::vault::indexing() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn keeping_a_page_is_a_change_so_it_asks_and_the_kept_page_is_found_afterwards() {
        let (host, vault, _) = rig("add");
        struct Say(Mutex<Vec<String>>, ApprovalOutcome);
        impl Approver for Say {
            fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
                self.0.lock().unwrap().push(req.summary);
                let a = self.1;
                Box::pin(async move { a })
            }
        }
        let say = Arc::new(Say(Mutex::new(vec![]), ApprovalOutcome::Approved(ApprovalScope::Once)));
        host.set_approver(say.clone());
        // A clean run keeps a page without a question: it only changes the vault, which is Aqua's own.
        let res = call(&host, "vault.add", json!({"title": "Rust 1.90", "text": "lld is the default linker now", "source": "https://blog.rust-lang.org"}), Caller::Agent, "r1");
        assert!(res.ok, "{}", res.text);
        assert!(say.0.lock().unwrap().is_empty(), "no question for a clean run");
        assert_eq!(vault.search("default linker", 3)[0].source, "saved page");
        // After text from outside was read, a change asks, and the person can say no: nothing is kept.
        let no = Arc::new(Say(Mutex::new(vec![]), ApprovalOutcome::Denied));
        host.set_approver(no.clone());
        host.mark_tainted("r2");
        assert!(!call(&host, "vault.add", json!({"title": "Other", "text": "never kept"}), Caller::Agent, "r2").ok);
        assert_eq!(no.0.lock().unwrap().clone(), vec!["Keep \"Other\" in the vault".to_string()]);
        assert!(vault.search("never kept", 3).is_empty());
        // The voice cannot change anything.
        assert!(!call(&host, "vault.add", json!({"title": "V", "text": "by voice"}), Caller::Voice, "r3").ok);
    }
}
