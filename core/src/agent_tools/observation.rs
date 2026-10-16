//! What the agent reads of a tool's result: the text with its notes, a refusal in the words the loop stops on, the person's own words with their "no", and the pages and searches that become the sources of the run (move only: this was part of `agent_tools.rs`).

/// The most characters of one warning the model is shown, and of all of them (card OA5-03, TL-03).
const WARNING_CHARS: usize = 200;
const WARNINGS_CHARS: usize = 400;

/// What a call that worked has to say besides its text, as one line the model reads after it: `[note: offset_line 0 was raised to 1; a search stopped at its size limit]`. The notes are
/// the host's own (a spelling read as another, a number moved into its bounds) and the provider's warnings; each is made of one line, with what cannot be seen taken out, and is cut at
/// 200 characters, and the line at 400 (the rest is not shown). `None`: nothing to say.
fn notes_line(warnings: &[String]) -> Option<String> {
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0usize;
    for w in warnings {
        let one: String = aqua_tool_host::output::sanitize(w).split_whitespace().collect::<Vec<_>>().join(" ");
        if one.is_empty() {
            continue;
        }
        let one: String = if one.chars().count() > WARNING_CHARS { format!("{}…", one.chars().take(WARNING_CHARS - 1).collect::<String>()) } else { one };
        if used + one.chars().count() > WARNINGS_CHARS && !shown.is_empty() {
            break;
        }
        used += one.chars().count() + 2;
        shown.push(one);
    }
    (!shown.is_empty()).then(|| format!("[note: {}]", shown.join("; ")))
}

/// The words that introduce what the person typed with their "no" in a refusal (card OA10-03, AG-11); the loop looks for them to tell a refusal that comes with an instruction.
pub const PERSON_SAID_MARK: &str = "They said, in their own words:";

/// Does this observation carry what the person typed with their "no"?
pub fn denial_carries_words(observation: &str) -> bool {
    observation.starts_with("ERROR [CONFIRMATION_DENIED]") && observation.contains(PERSON_SAID_MARK)
}

/// What the agent loop sees for a tool result: the masked, labelled text, with failures in the
/// `ERROR [CODE] tool: message` form the loop already knows how to stop on.
pub fn observation(res: &aqua_tool_host::ToolResult) -> String {
    use aqua_tool_host::ErrorCode;
    if res.ok {
        let text = if res.text.trim().is_empty() { "Done.".to_string() } else { res.text.clone() };
        return match notes_line(&res.warnings) {
            Some(note) => format!("{text}\n{note}"),
            None => text,
        };
    }
    let Some(e) = res.error.as_ref() else { return res.text.clone() };
    match e.code {
        // The user said no (or did not answer): the loop must stop and say so, not look for another way.
        ErrorCode::ApprovalDenied | ErrorCode::ApprovalTimeout | ErrorCode::ApprovalRequired => {
            // A "no" that came with words (card OA10-03, AG-11): the words are the person's own, shown as such and not as a rule, and they do not approve the refused action. Only a
            // denial carries them (a timeout has none), and only the host puts them there.
            if let (ErrorCode::ApprovalDenied, Some(said)) = (e.code, e.person_said()) {
                let said = said.split_whitespace().collect::<Vec<_>>().join(" ").replace('"', "'");
                return format!("ERROR [CONFIRMATION_DENIED] {}: The user did not approve: {}. {PERSON_SAID_MARK} \"{said}\". That is what they want instead; it does not approve the refused action. Do not repeat the refused action.", res.tool, e.message.trim_end_matches('.'));
            }
            format!("ERROR [CONFIRMATION_DENIED] {}: The user did not approve: {}. Do not retry this action; tell the user.", res.tool, e.message.trim_end_matches('.'))
        }
        // A path outside the allowed folders is a wrong guess, not a decision to route around: say so
        // and let the model try again with a path inside them (the retry is checked the same way).
        ErrorCode::PermissionDenied if e.message.contains("outside the folders Aqua may use") => {
            format!(
                "ERROR [INVALID_ARGUMENTS] {}: {} If the person named this folder in their request, call fs.allow_folder with it (they are asked); otherwise use a path inside the allowed folders.",
                res.tool, e.message
            )
        }
        // Policy, secrets, network rules, protected branches, host keys: no other route exists.
        c if c.stops_run() && c != ErrorCode::Cancelled && c != ErrorCode::PermissionDenied => {
            format!("ERROR [PERMISSION_DENIED] {}: ({}) {}", res.tool, c.as_str(), e.message)
        }
        _ if res.text.starts_with("ERROR") => res.text.clone(),
        c => format!("ERROR [{}] {}: {}", c.as_str(), res.tool, res.text),
    }
}

/// Renumbers the lines of a `web.search` list (`[1] ...`, `[2] ...`) to the numbers the run's sources have, so that a number names one page for the
/// whole run. Only a line that begins with `[n] ` is touched: a result's words are on the same line after the domain.
fn renumber_search_lines(text: &str, ids: &[usize]) -> String {
    text.lines()
        .map(|line| {
            let renumbered = line.strip_prefix('[').and_then(|rest| rest.split_once("] ")).and_then(|(n, tail)| {
                let n: usize = n.parse().ok()?;
                let id = *ids.get(n.checked_sub(1)?)?;
                Some(format!("[{id}] {tail}"))
            });
            renumbered.unwrap_or_else(|| line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a web tool brought back becomes a source of this run (and the search list gets its run-wide numbers).
pub(super) fn note_web_result(res: &aqua_tool_host::ToolResult, run_id: &str, read_for: &str) -> Option<String> {
    if !res.ok || run_id.is_empty() {
        return None;
    }
    match res.tool.as_str() {
        "web.search" => {
            let results = res.data["results"].as_array()?;
            let ids: Vec<usize> = results
                .iter()
                .map(|r| crate::sources::add_hit(run_id, r["url"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or(""), r["date"].as_str(), r["snippet"].as_str().unwrap_or("")))
                .collect();
            Some(renumber_search_lines(&res.text, &ids))
        }
        "web.read" | "web.fetch" => {
            let url = res.data["url"].as_str()?;
            let (nonce, body) = match aqua_tool_host::output::split_untrusted(&res.text) {
                Some((nonce, inner)) => (Some(nonce.to_string()), inner),
                None => (None, res.text.as_str()),
            };
            let title = res.data["title"].as_str().unwrap_or("");
            let id = crate::sources::add_page(run_id, url, title, res.data["date"].as_str(), body);
            // With the reader on, the agent is not given the page: a separate model call with no tools reads it and the agent gets the facts it can
            // quote. A reader that fails leaves the agent no page at all (never the page itself).
            if crate::modules::reader::enabled() {
                let goal = if read_for.trim().is_empty() { crate::sources::goal_of(run_id) } else { read_for.to_string() };
                let shown = match crate::modules::reader::read(&goal, body) {
                    Ok(facts) => crate::modules::reader::render(id, title, url, &facts),
                    Err(e) => return Some(format!("ERROR [BACKEND_ERROR] {}: the page could not be read by the reader ({e})", res.tool)),
                };
                // The facts are text written from an untrusted page: they stay inside the same kind of fence the page was in.
                return Some(match nonce {
                    Some(n) => aqua_tool_host::output::label_untrusted(&shown, &n),
                    None => shown,
                });
            }
            None
        }
        _ => None,
    }
}

/// Company graph records are first-class sources too. Register each source URI and the exact
/// connected record with the run's existing citation verifier, then tell the planner which
/// numeric citation maps to which stable graph source ID.
pub(super) fn note_company_result(res: &aqua_tool_host::ToolResult, run_id: &str) -> Option<String> {
    if !res.ok || !matches!(res.tool.as_str(), "company.search_context" | "company.explain_path" | "company.propose_assignment") || run_id.is_empty() {
        return None;
    }
    let records = res.data.get("records")?.as_array()?;
    let mut mappings: Vec<String> = Vec::new();
    for record in records {
        let source_id = record.get("source_id").and_then(serde_json::Value::as_str).unwrap_or("");
        let uri = record.get("source_uri").and_then(serde_json::Value::as_str).unwrap_or("");
        if source_id.is_empty() || uri.is_empty() {
            continue;
        }
        let title = record.get("source_title").and_then(serde_json::Value::as_str).unwrap_or(source_id);
        let evidence = serde_json::to_string(record).unwrap_or_default();
        let id = crate::sources::add_page(run_id, uri, title, None, &evidence);
        let mapping = format!("[{id}] {source_id} ({title})");
        if !mappings.contains(&mapping) {
            mappings.push(mapping);
        }
    }
    if mappings.is_empty() {
        return Some(observation(res));
    }
    Some(format!("{}\nCitation mapping (use these numeric citations in the final answer):\n{}", observation(res), mappings.join("\n")))
}
