//! "Index my documents folder", said aloud (card R6-15): the vault could be started only by a web request, with no screen, no command and no phrase.
//!
//! The program answers this phrase itself, with no model, and **it reads nothing**: it puts a question on the screen, through the tool host, so that it is the same
//! question as any other (the exact folder, always asked, written to the audit with how it was answered), and the folder is read only if the person says yes there.
//! A voice cannot change anything (the policy refuses it), which is why the question is asked of the person's screen and not of the voice. The phrases are exact
//! ones: "search my documents for ..." is a search and goes on to the agent.

use aqua_tool_host::{Caller, RequestContext, ToolRequest};
use serde_json::json;

/// The words of a request: lower case, letters and digits and apostrophes, one space between words.
fn words(text: &str) -> String {
    let t: String = text.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' }).collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is this the person asking for their Documents folder to be read into the vault? Only the plain phrases, with a polite word or two around them.
pub fn asks_to_index_documents(text: &str) -> bool {
    let mut t = words(text);
    // Polite words in front, in any order and as many as there are.
    loop {
        let mut changed = false;
        for polite in ["please ", "can you ", "could you ", "would you ", "will you ", "i want you to ", "i would like you to ", "i'd like you to ", "go ahead and ", "now "] {
            if let Some(rest) = t.strip_prefix(polite).map(str::to_string) {
                t = rest;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let t = t.trim_end_matches(" please").trim_end_matches(" now");
    matches!(
        t,
        "index my documents"
            | "index my documents folder"
            | "index the documents folder"
            | "index my document folder"
            | "start indexing my documents"
            | "start indexing my documents folder"
            | "reindex my documents"
            | "reindex my documents folder"
            | "add my documents folder to the vault"
            | "add my documents to the vault"
            | "put my documents in the vault"
            | "put my documents folder in the vault"
    )
}

/// The reply to the phrase, or `None` when the words are not that request. The question is put on the screen on a thread of its own (it waits for the answer, up to the
/// time the host allows), so the reply is not held up by it.
pub fn handle_utterance(text: &str) -> Option<String> {
    if !asks_to_index_documents(text) {
        return None;
    }
    let Some(folder) = dirs::document_dir() else { return Some("I cannot find your Documents folder on this computer.".into()) };
    let Some(host) = crate::agent_tools::host().cloned() else { return Some("The tool host is not running, so I cannot ask you.".into()) };
    let path = folder.to_string_lossy().into_owned();
    let started = std::thread::Builder::new().name("aqua-vault-ask".into()).spawn(move || {
        // The model did not choose this and the voice may not do it: it is the program's own question, always asked, and the answer is the person's, on the screen.
        let context = RequestContext::new(Caller::Agent);
        let result = host.call_blocking(ToolRequest { tool: "vault.add_folder".into(), arguments: json!({ "path": path }), context });
        if !result.ok {
            tracing::info!("the vault folder was not added: {}", result.text.lines().next().unwrap_or(""));
        }
    });
    Some(if started.is_ok() {
        "I put a question on your screen: may I read your Documents folder and keep a search index of it? Nothing is read unless you say yes there.".into()
    } else {
        "I could not put the question on your screen.".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plain_phrases_are_the_request_and_nothing_near_them_is() {
        for yes in [
            "index my documents folder",
            "Index my documents folder.",
            "please index my documents",
            "Can you index my documents folder?",
            "could you please index the documents folder",
            "start indexing my documents folder",
            "add my documents folder to the vault",
            "put my documents in the vault now",
            "I'd like you to reindex my documents",
        ] {
            assert!(asks_to_index_documents(yes), "{yes}");
        }
        for no in [
            "search my documents for invoices",
            "what is in my documents folder",
            "index cards",
            "do not index my documents folder",
            "index my documents folder and send it to bob",
            "index my downloads folder",
            "open my documents folder",
            "delete my documents",
            "index my documents folder in the vault of someone else",
            "",
        ] {
            assert!(!asks_to_index_documents(no), "{no}");
        }
        assert_eq!(handle_utterance("what time is it"), None, "any other request goes on");
    }
}
