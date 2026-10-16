//! Self-Correction Reflection Engine & Crash Memory Logger
//!
//! Captures runtime task crashes into `.aqua/crash_reports/{task_id}.json`, synthesizes
//! self-correction repair strategies, and prevents Aqua from repeating past failure patterns.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Structure representing a captured runtime crash snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashReport {
    pub task_id: String,
    pub timestamp: String,
    pub error_message: String,
    pub attempted_action: String,
    pub repaired_action: String,
    pub auto_repaired: bool,
}

/// Self-Correction Reflection Engine.
pub struct ReflectionEngine {
    crash_dir: PathBuf,
}

/// How many crash reports are kept.
const MAX_REPORTS: usize = 50;

impl ReflectionEngine {
    /// Creates a new ReflectionEngine instance.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let crash_dir = base_dir.as_ref().join(".aqua").join("crash_reports");
        Self { crash_dir }
    }

    /// Logs a runtime task crash snapshot to disk.
    pub fn log_crash_report(
        &self,
        task_id: &str,
        error_message: &str,
        attempted_action: &str,
    ) -> Result<CrashReport, String> {
        fs::create_dir_all(&self.crash_dir)
            .map_err(|e| format!("Failed to create crash directory: {e}"))?;

        let report_file = self.crash_dir.join(format!("{task_id}.json"));
        let now = utc_now();

        let temp_crash = CrashReport {
            task_id: task_id.to_string(),
            timestamp: now,
            // Error text can quote paths, addresses and page content; the report keeps a redacted, bounded copy.
            error_message: crate::memory::experience::sanitize::redact(error_message).0.chars().take(500).collect(),
            attempted_action: crate::memory::experience::sanitize::redact(attempted_action).0.chars().take(300).collect(),
            repaired_action: String::new(),
            auto_repaired: false,
        };

        let repaired_action = self.synthesize_repair_strategy(&temp_crash);

        let final_report = CrashReport {
            repaired_action,
            auto_repaired: true,
            ..temp_crash
        };

        let json = serde_json::to_string_pretty(&final_report)
            .map_err(|e| format!("Failed to serialize crash report: {e}"))?;
        fs::write(&report_file, json)
            .map_err(|e| format!("Failed to write crash report file: {e}"))?;

        tracing::info!(
            "Logged runtime crash report & repair strategy for task '{}'",
            task_id
        );
        self.prune(MAX_REPORTS);
        Ok(final_report)
    }

    /// Keep only the newest `keep` reports. Every agent run reads them all and one is written per error, so without a
    /// limit the folder (and every run) only grows.
    fn prune(&self, keep: usize) {
        let Ok(entries) = fs::read_dir(&self.crash_dir) else { return };
        let mut files: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .filter_map(|p| fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
            .collect();
        if files.len() <= keep {
            return;
        }
        files.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, old) in files.into_iter().skip(keep) {
            let _ = fs::remove_file(old);
        }
    }

    /// Advice for the planner after a failed call. It is true for the error that happened and never names a module or a
    /// privilege that does not exist: the old table told the model to "invoke the Autonomous Dependency Integrator" (a stub)
    /// after any "not found", to "request elevated privilege" after a refusal, and to retry `App` with "Notepad" after any
    /// error that merely contained the letters "app". Only entries that were checked against the real tools are specific;
    /// everything else is one generic sentence.
    pub fn synthesize_repair_strategy(&self, crash: &CrashReport) -> String {
        let err = crash.error_message.to_lowercase();
        let action = crash.attempted_action.to_ascii_lowercase();
        let tool = crash.attempted_action.as_str();
        let is_browser_tool = action.starts_with("browser_") || action.starts_with("browser.");
        if action == "fs.edit" && (err.contains("not found") || err.contains("did not match") || err.contains("no match")) {
            "The text to replace was not in the file: read the file again with fs.read and copy the exact text, including spaces and line breaks."
                .to_string()
        } else if err.contains("timeout") || err.contains("timed out") {
            format!("'{tool}' timed out. Try once more with a smaller request or a longer wait; do not repeat the same call more than once.")
        } else if err.contains("permission_denied") || err.contains("permission denied") || err.contains("access denied") || err.contains("refused by") {
            format!("'{tool}' was refused. Do not retry it or look for a way round: tell the user what was refused and why.")
        } else if action.starts_with("browser_")
            && (err.contains("element_not_found") || err.contains("does not match any element") || err.contains("stale_ref") || err.contains("not found in the current page"))
        {
            // The page is there; the element the call named is not on it (a name the model made up, or a page that changed).
            format!(
                "'{tool}' did not find that element on the page. Take a browser_snapshot and use the ref of the element you need (like e55) as the target, never a name you \
made up. If the page is asking the person to sign in, or has no such place, say that in final instead of trying again."
            )
        } else if err.contains("browser_not_running")
            || err.contains("browser_crashed")
            || err.contains("chrome is not installed")
            // "extension" and "not connected" say something about the browser only when a browser tool said them: a file with an unsupported extension, or an ssh or database
            // tool that is not connected, is a recoverable error of its own and must not be told to give up.
            || (is_browser_tool && (err.contains("not connected") || err.contains("extension")))
            || (err.contains("browser") && (err.contains("not available") || err.contains("not installed") || err.contains("not running")))
        {
            format!("The browser could not be used for '{tool}'. Do not look for another way round: tell the user what was not available.")
        } else if action.starts_with("browser_") {
            format!("'{tool}' failed. Take a browser_snapshot to see the page as it is, then change the arguments; do not repeat the same call or open the same page again.")
        } else if action == "app" && (err.contains("launch") || err.contains("not found") || err.contains("could not")) {
            "Retry App with the exact name shown in the Start menu. If App keeps failing, tell the user which app could not be started."
                .to_string()
        } else if err.contains("not found") || err.contains("no such") || err.contains("does not exist") {
            format!("'{tool}' could not find what it was given. Check the name or path with a find or list tool before trying again.")
        } else {
            format!("'{tool}' failed. Change the arguments or use another tool; do not repeat the same call.")
        }
    }

    /// Lists all historical crash reports for reflection analysis.
    pub fn list_crash_history(&self) -> Result<Vec<CrashReport>, String> {
        if !self.crash_dir.exists() {
            return Ok(vec![]);
        }

        let entries = fs::read_dir(&self.crash_dir)
            .map_err(|e| format!("Failed to read crash directory: {e}"))?;
        let mut reports = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(report) = serde_json::from_str::<CrashReport>(&content) {
                        reports.push(report);
                    }
                }
            }
        }

        Ok(reports)
    }
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod hint_and_pruning_tests {
    use super::*;

    #[test]
    fn no_repair_hint_points_at_a_retired_tool() {
        let dir = std::env::temp_dir().join(format!("aqua_refl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let engine = ReflectionEngine::new(&dir);
        for (action, err) in [("browser_navigate", "extension not connected"), ("App", "could not launch"), ("x", "timed out"), ("y", "nothing special")] {
            let report = CrashReport {
                task_id: "t".into(), timestamp: "0".into(), error_message: err.into(), attempted_action: action.into(),
                repaired_action: String::new(), auto_repaired: false,
            };
            let hint = engine.synthesize_repair_strategy(&report);
            for retired in crate::memory::solutions_db::RETIRED_TOOLS {
                assert!(!hint.contains(retired), "{action}/{err}: {hint}");
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The old table answered every "not found" with "Invoke Autonomous Dependency Integrator" (a stub), every refusal with
    /// "request elevated privilege", and any error containing the letters "app" with "Retry App ... Notepad".
    #[test]
    fn hints_are_true_for_the_error_and_name_no_module_that_does_not_exist() {
        let engine = ReflectionEngine::new(std::env::temp_dir().join("aqua_refl_hints"));
        let hint = |action: &str, err: &str| {
            engine.synthesize_repair_strategy(&CrashReport {
                task_id: "t".into(), timestamp: "0".into(), error_message: err.into(), attempted_action: action.into(),
                repaired_action: String::new(), auto_repaired: false,
            })
        };
        let not_found = hint("fs.read", "ERROR [NOT_FOUND] fs.read: no such file");
        assert!(!not_found.contains("Dependency Integrator") && not_found.contains("find or list"), "{not_found}");
        let refused = hint("term.run", "ERROR [PERMISSION_DENIED] term.run: refused by policy");
        assert!(!refused.to_lowercase().contains("elevated") && refused.contains("tell the user"), "{refused}");
        let edit = hint("fs.edit", "the text to replace was not found");
        assert!(edit.contains("fs.read"), "{edit}");
        let happened = hint("web.fetch", "something happened to the request");
        assert!(!happened.contains("Notepad") && happened.contains("do not repeat"), "{happened}");
        let app = hint("App", "could not launch the application");
        assert!(app.contains("Start menu") && !app.contains("Notepad"), "{app}");
    }

    #[test]
    fn a_browser_error_is_told_for_what_it_is_and_never_as_a_browser_that_was_not_available() {
        let engine = ReflectionEngine::new(std::env::temp_dir().join("aqua_refl_browser_hints"));
        let hint = |action: &str, err: &str| {
            engine.synthesize_repair_strategy(&CrashReport {
                task_id: "t".into(), timestamp: "0".into(), error_message: err.into(), attempted_action: action.into(),
                repaired_action: String::new(), auto_repaired: false,
            })
        };
        // The element is not on the page: take a snapshot and use a ref; and say so when the page wants a sign-in. No App, no 'not available'.
        let missing = hint("browser_type", "ERROR [ELEMENT_NOT_FOUND] browser_type: \"post_text_area\" does not match any elements.");
        assert!(missing.contains("browser_snapshot") && missing.contains("ref") && missing.contains("sign in"), "{missing}");
        assert!(!missing.contains("App tool") && !missing.contains("not available"), "{missing}");
        let stale = hint("browser_click", "ERROR [STALE_REF] browser_click: Ref e5 not found in the current page snapshot");
        assert!(stale.contains("browser_snapshot") && !stale.contains("App tool"), "{stale}");
        // The browser really is not there: say so, and do not send the model to another way round.
        let gone = hint("browser_navigate", "ERROR [BROWSER_NOT_RUNNING] browser_navigate: the browser is not running");
        assert!(gone.contains("could not be used") && gone.contains("tell the user") && !gone.contains("App tool"), "{gone}");
        // Any other browser error: look at the page, change the call, do not open the same page again.
        let other = hint("browser_click", "ERROR [BACKEND_ERROR] browser_click: something odd");
        assert!(other.contains("browser_snapshot") && other.contains("same page"), "{other}");
    }

    #[test]
    fn an_error_that_merely_says_extension_or_not_connected_is_not_a_browser_that_was_missing_unless_a_browser_tool_said_it() {
        let engine = ReflectionEngine::new(std::env::temp_dir().join("aqua_refl_extension_hints"));
        let hint = |action: &str, err: &str| {
            engine.synthesize_repair_strategy(&CrashReport {
                task_id: "t".into(), timestamp: "0".into(), error_message: err.into(), attempted_action: action.into(),
                repaired_action: String::new(), auto_repaired: false,
            })
        };
        // A file of a kind the reader does not know is a recoverable error: it must not tell the model to give up and tell the user about a browser.
        let unsupported = hint("fs.read", "ERROR [UNSUPPORTED] fs.read: unsupported file extension .xyz");
        assert!(!unsupported.contains("browser") && !unsupported.contains("could not be used"), "{unsupported}");
        assert!(unsupported.contains("fs.read") && unsupported.contains("Change the arguments"), "{unsupported}");
        // The same for a remote shell or a database that has lost its connection.
        for (tool, err) in [("ssh.exec", "ERROR [UNAVAILABLE] ssh.exec: not connected to the host"), ("db.query", "ERROR [UNAVAILABLE] db.query: not connected")] {
            let h = hint(tool, err);
            assert!(!h.contains("browser") && !h.contains("could not be used"), "{tool}: {h}");
        }
        // A browser tool that says the extension is not connected is still told the browser could not be used.
        for err in ["extension not connected", "ERROR [BACKEND_ERROR] browser_click: the Browser MCP extension is not connected"] {
            let h = hint("browser_click", err);
            assert!(h.contains("could not be used") && h.contains("tell the user"), "{err}: {h}");
        }
        // And a browser that is really not running is told so whichever tool found it out, because the host's code says it.
        assert!(hint("local_open_url", "ERROR [BROWSER_NOT_RUNNING] local_open_url: the browser is not running").contains("could not be used"));
    }

    #[test]
    fn only_the_newest_reports_are_kept() {
        let dir = std::env::temp_dir().join(format!("aqua_refl_prune_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let engine = ReflectionEngine::new(&dir);
        for i in 0..(MAX_REPORTS + 8) {
            engine.log_crash_report(&format!("task{i}"), "boom", "act").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let kept = engine.list_crash_history().unwrap();
        assert_eq!(kept.len(), MAX_REPORTS);
        assert!(kept.iter().any(|r| r.task_id == format!("task{}", MAX_REPORTS + 7)), "the newest is kept");
        assert!(!kept.iter().any(|r| r.task_id == "task0"), "the oldest is gone");
        let _ = fs::remove_dir_all(&dir);
    }
}
