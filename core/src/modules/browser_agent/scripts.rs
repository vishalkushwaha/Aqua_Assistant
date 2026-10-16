//! The goal-specific scripts of the loop: what is done with no planning round for a goal that names a site to open, an e-mail to send, a search to make or a plain app to start, the headlines of a news page, and the way round when an app will not start (card OA3-05, move only).

use super::*;

/// Known website goals when a Start-menu App launch fails.
pub(super) fn web_fallback_url(goal: &str) -> Option<&'static str> {
    // Whole words only: "insta" must not match "install", "maps" must not match "mapsy", "x.com" must not match "box.com".
    let words = goal_words(goal);
    const MAP: &[(&[&str], &str)] = &[
        (&["youtube"], "https://www.youtube.com"),
        (&["instagram", "insta"], "https://www.instagram.com"),
        (&["gmail", "google mail"], "https://mail.google.com"),
        (&["google maps", "maps"], "https://maps.google.com"),
        (&["twitter", "x.com"], "https://x.com"),
        (&["facebook"], "https://www.facebook.com"),
        (&["reddit"], "https://www.reddit.com"),
        (&["netflix"], "https://www.netflix.com"),
        (&["spotify"], "https://open.spotify.com"),
        (&["github"], "https://github.com"),
        (&["linkedin"], "https://www.linkedin.com"),
        (&["whatsapp"], "https://web.whatsapp.com"),
        (&["tiktok"], "https://www.tiktok.com"),
        (&["discord"], "https://discord.com/app"),
        (&["telegram"], "https://web.telegram.org"),
        (&["twitch"], "https://www.twitch.tv"),
        (&["amazon"], "https://www.amazon.com"),
    ];
    for (keys, url) in MAP {
        if keys.iter().any(|k| words.contains(&format!(" {k} "))) {
            return Some(*url);
        }
    }
    None
}

/// The goal as lower-case words separated by single spaces, with a space at both ends, so that " name " finds a
/// whole word or phrase and nothing inside another word. Dots stay inside a word ("x.com").
fn goal_words(goal: &str) -> String {
    let cleaned: String = goal.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '.' { c } else { ' ' }).collect();
    let words: Vec<&str> = cleaned.split_whitespace().map(|w| w.trim_matches('.')).filter(|w| !w.is_empty()).collect();
    format!(" {} ", words.join(" "))
}

/// May this address be opened in the person's own browser? Same site rules and the same question as
/// `browser_navigate`. With no browser handle to consult, the built-in default rules apply and an "ask" cannot be
/// asked, so it is refused.
pub(super) fn authorize_local_open(browser: &Option<BrowserHandle>, url: &str) -> Result<(), String> {
    match browser.as_ref() {
        Some(b) => b.authorize_open_url(url),
        None => match aqua_mcp_adapters::browser::BrowserPolicyConfig::default().check_url(url) {
            aqua_mcp_adapters::browser::UrlDecision::Allow => Ok(()),
            aqua_mcp_adapters::browser::UrlDecision::Deny(why) => Err(format!("Blocked by browser policy: {why}")),
            aqua_mcp_adapters::browser::UrlDecision::Ask { origin, .. } => Err(format!("{origin} is not on the allowed sites list and there is no way to ask you here")),
        },
    }
}

/// True when the goal asks for work beyond merely opening/visiting a site.
pub(super) fn goal_has_follow_on_work(goal: &str) -> bool {
    let g = goal.to_ascii_lowercase();
    const ACTION_MARKERS: &[&str] = &[
        "send",
        "sent",
        "compose",
        "write",
        "type ",
        "typed",
        "click",
        "search",
        "find ",
        "download",
        "upload",
        "fill",
        "login",
        "log in",
        "sign in",
        "signin",
        "reply",
        "forward",
        "delete",
        "create",
        "post ",
        "tweet",
        "message",
        "buy ",
        "order",
        "book ",
        "subscribe",
        "play ",
        "like ",
        "comment",
        "share",
        "submit",
        "draft",
        "attach",
        "email",
        "mail to",
        " a mail",
        "recipient",
        "subject",
        "body",
        "saying",
        "tell them",
        "ask ",
        "how are you",
    ];
    if ACTION_MARKERS.iter().any(|m| g.contains(m)) {
        return true;
    }
    // Recipient / quoted body are strong compose signals.
    if g.contains('@') {
        return true;
    }
    if (g.contains('\'') || g.contains('"') || g.contains('“') || g.contains('”'))
        && (g.contains(" and ") || g.contains(" then ") || g.contains(" saying"))
    {
        return true;
    }
    false
}

/// Open/visit a known site and nothing else (safe to short-circuit after navigation).
pub(super) fn goal_is_navigation_only(goal: &str) -> bool {
    web_fallback_url(goal).is_some() && !goal_has_follow_on_work(goal)
}

pub(super) fn answer_only_claims_navigation(answer: &str) -> bool {
    let a = answer.to_ascii_lowercase();
    let nav = [
        "opened",
        "launched",
        "navigated",
        "in your default browser",
        "is open",
        "has been opened",
        "brought up",
    ];
    let deeper = [
        "sent",
        "composed",
        "typed",
        "emailed",
        "submitted",
        "downloaded",
        "posted",
        "replied",
        "shared",
        "filled",
        "clicked",
        "searched",
        "message delivered",
        "email sent",
    ];
    nav.iter().any(|p| a.contains(p)) && !deeper.iter().any(|p| a.contains(p))
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(super) fn extract_email_addr(goal: &str) -> Option<String> {
    let bytes = goal.as_bytes();
    let at = goal.find('@')?;
    let mut start = at;
    while start > 0 {
        let c = bytes[start - 1] as char;
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '+' || c == '-' {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = at + 1;
    while end < bytes.len() {
        let c = bytes[end] as char;
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
            end += 1;
        } else {
            break;
        }
    }
    let addr = goal[start..end].trim();
    if addr.contains('.') && addr.len() > 5 {
        Some(addr.to_string())
    } else {
        None
    }
}

pub(super) fn extract_mail_body(goal: &str) -> Option<String> {
    let lower = goal.to_ascii_lowercase();
    if let Some(i) = lower.find("saying") {
        let mut rest = goal[i + 6..].trim();
        rest = rest.trim_start_matches([':', ' ', '\t']);
        if let Some(q) = rest.chars().next().filter(|c| matches!(c, '\'' | '"')) {
            rest = rest.trim_start_matches(q);
            if let Some(end) = rest.find(q) {
                rest = &rest[..end];
            }
        }
        let cleaned = rest
            .trim()
            .trim_end_matches(" for me")
            .trim_end_matches(" please")
            .trim();
        if cleaned.len() >= 2 {
            return Some(cleaned.to_string());
        }
    }
    if let Some(email) = extract_email_addr(goal) {
        if let Some(pos) = goal.find(&email) {
            let mut after = goal[pos + email.len()..].trim();
            after = after.trim_start_matches([':', ',', '-', ' ']);
            let cleaned = after
                .trim_end_matches(" for me")
                .trim_end_matches(" please")
                .trim();
            if cleaned.len() >= 2
                && !cleaned.eq_ignore_ascii_case("and")
                && !cleaned.eq_ignore_ascii_case("to")
            {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

pub(super) fn is_email_send_goal(goal: &str) -> bool {
    extract_email_addr(goal).is_some()
        && goal_has_follow_on_work(goal)
        && {
            let g = goal.to_ascii_lowercase();
            g.contains("mail")
                || g.contains("email")
                || g.contains("send")
                || g.contains("sent")
                || g.contains("compose")
                || g.contains("message")
        }
}

/// Prefill a compose window (Gmail deep-link or mailto) — general email-send bootstrap.
pub(super) fn scripted_email_compose_action(
    goal: &str,
    step: u32,
    history: &[(String, String)],
    tools: &[McpTool],
) -> Option<(String, Value)> {
    if !is_email_send_goal(goal) {
        return None;
    }
    if email_compose_opened(history) {
        return None;
    }
    // Step 1 always; or step 2 right after an inbox-only open.
    let inbox_opened = history.iter().any(|(t, o)| {
        let tl = t.to_ascii_lowercase();
        (tl == "local_open_url" || tl == "browser_navigate" || tl == "powershell")
            && o.to_ascii_lowercase().contains("mail.google")
            && !o.to_ascii_lowercase().contains("view=cm")
    });
    if step != 1 && !(step == 2 && inbox_opened) {
        return None;
    }
    let email = extract_email_addr(goal)?;
    let body = extract_mail_body(goal).unwrap_or_else(|| "Hello".to_string());
    let g = goal.to_ascii_lowercase();
    let url = if g.contains("gmail") || g.contains("google mail") {
        format!(
            "https://mail.google.com/mail/?view=cm&fs=1&to={}&body={}",
            url_encode(&email),
            url_encode(&body)
        )
    } else {
        format!("mailto:{}?body={}", url_encode(&email), url_encode(&body))
    };
    // Prefer Browser MCP navigate when extension is connected.
    if tools.iter().any(|t| t.name == "browser_navigate") {
        return Some(("browser_navigate".into(), json!({ "url": url })));
    }
    Some(("local_open_url".into(), json!({ "url": url })))
}

pub(super) fn email_compose_opened(history: &[(String, String)]) -> bool {
    history.iter().any(|(t, o)| {
        if observation_indicates_failure(o) {
            return false;
        }
        let l = o.to_ascii_lowercase();
        let tl = t.to_ascii_lowercase();
        l.contains("view=cm")
            || l.contains("mailto:")
            || l.contains("compose mail")
            // browser_navigate observations often omit the raw view=cm query string
            || ((tl == "browser_navigate" || tl == "local_open_url")
                && (l.contains("mail.google") || l.contains("compose")))
    })
}

/// True once we have attempted Ctrl+Enter / Send after compose was opened (with focus).
/// A tool result that is an error, a refusal or a block: whatever the tool was meant to do did not happen.
fn observation_is_failure(obs: &str) -> bool {
    let t = obs.trim_start();
    t.starts_with("ERROR")
        || ["[POLICY_", "[PERMISSION_DENIED]", "[CONFIRMATION_DENIED]", "[UNAVAILABLE]", "[INVALID_ARGUMENTS]", "[BROWSER_", "[EXTENSION_NOT_CONNECTED]", "[TAB_LIMIT]"]
            .iter()
            .any(|m| obs.contains(m))
}

/// Did something actually say the message went? ("Message sent", "email sent" in a result that did not fail.)
pub(super) fn email_send_confirmed(history: &[(String, String)]) -> bool {
    history.iter().any(|(_, o)| {
        let l = o.to_ascii_lowercase();
        !observation_is_failure(o) && (l.contains("message sent") || l.contains("email sent"))
    })
}

pub(super) fn email_send_attempted(history: &[(String, String)]) -> bool {
    let mut compose_seen = false;
    for (t, o) in history {
        // A key press that was refused or failed (a voice request may not press keys; the browser was offline; the
        // desktop guard said no) sent nothing, so it is not an attempt to send.
        if observation_is_failure(o) {
            continue;
        }
        let l = o.to_ascii_lowercase();
        if l.contains("view=cm") || l.contains("mailto:") || l.contains("compose mail") {
            compose_seen = true;
        }
        if !compose_seen {
            continue;
        }
        // Focus failures must NOT count — allow a focused retry.
        if l.contains("not sent")
            || l.contains("window not focused")
            || l.contains("could not focus gmail")
        {
            continue;
        }
        let tl = t.to_ascii_lowercase();
        if tl == "shortcut"
            || tl == "browser_press_key"
            || (tl == "browser_click" && (l.contains("send") || l.contains("clicked") || l.len() > 2))
            || (tl == "click" && (l.contains("send") || l.contains("clicked")))
            || l.contains("message sent")
            || l.contains("email sent")
            || (l.contains("ctrl+enter") && l.contains("sendkeys attempted"))
            || (tl == "powershell" && l.contains("sendkeys attempted") && l.contains("focused="))
        {
            return true;
        }
    }
    false
}

pub(super) fn email_send_focus_looks_wrong(history: &[(String, String)]) -> bool {
    history.iter().rev().take(3).any(|(t, o)| {
        if !t.eq_ignore_ascii_case("Snapshot") && !t.eq_ignore_ascii_case("Screenshot") {
            return false;
        }
        let l = o.to_ascii_lowercase();
        // Prefer the Focused Window section when present.
        let focused = if let Some(idx) = l.find("focused window") {
            &l[idx..idx.saturating_add(400).min(l.len())]
        } else {
            l.as_str()
        };
        let focused_gmail = focused.contains("gmail")
            || focused.contains("mail.google")
            || focused.contains("compose");
        let focused_chrome = focused.contains("chrome") || focused.contains("google chrome");
        focused_chrome && !focused_gmail
    })
}

/// After a wrong-focus send we insert `mail_send_refocus` — allow one more scripted send after that.
pub(super) fn email_send_needs_refocus_retry(history: &[(String, String)]) -> bool {
    let Some(pos) = history
        .iter()
        .rposition(|(t, _)| t.eq_ignore_ascii_case("mail_send_refocus"))
    else {
        return false;
    };
    !history[pos + 1..].iter().any(|(_, o)| {
        let l = o.to_ascii_lowercase();
        l.contains("sendkeys attempted (focused=") && !l.contains("not sent")
    })
}

pub(super) fn email_send_final_answer(goal: &str, confirmed: bool) -> String {
    let email = extract_email_addr(goal).unwrap_or_else(|| "recipient".into());
    let body = extract_mail_body(goal).unwrap_or_else(|| "Hello".into());
    if confirmed {
        format!("Sent: Gmail confirmed the message to {email} with the text \"{body}\".")
    } else {
        // Never "sent" without something saying so: a key press that did nothing looks the same as one that worked.
        format!(
            "I opened Gmail compose to {email} with the text \"{body}\" and pressed Ctrl+Enter, but I did not see Gmail \
confirm that it was sent, so I cannot tell you it was. Please check your Sent folder."
        )
    }
}

pub(super) fn email_send_focus_fail_count(history: &[(String, String)]) -> usize {
    history
        .iter()
        .filter(|(_, o)| {
            let l = o.to_ascii_lowercase();
            l.contains("window not focused") || l.contains("ctrl+enter not sent")
        })
        .count()
}

/// After compose is open, send via Ctrl+Enter (Gmail) or click Send — skips unreliable chatty LLM.
pub(super) fn scripted_mail_send_action(
    goal: &str,
    step: u32,
    history: &[(String, String)],
    tools: &[McpTool],
) -> Option<(String, Value)> {
    if !is_email_send_goal(goal) || step < 2 {
        return None;
    }
    if !email_compose_opened(history) {
        return None;
    }
    if email_send_attempted(history) && !email_send_needs_refocus_retry(history) {
        return None;
    }
    // Don't spin forever if Gmail never comes to the foreground.
    if email_send_focus_fail_count(history) >= 2 {
        return None;
    }
    // Prefer Browser MCP when connected; fall back to the guarded desktop Shortcut/Click tools.
    // browser_click needs a snapshot `ref`, so use browser_press_key for Send.
    if tools.iter().any(|t| t.name == "browser_press_key") {
        return Some((
            "browser_press_key".into(),
            json!({ "key": "Control+Enter" }),
        ));
    }
    if tools.iter().any(|t| t.name == "Shortcut") {
        return Some((
            "Shortcut".into(),
            json!({ "shortcut": "ctrl+enter" }),
        ));
    }
    if tools.iter().any(|t| t.name == "Click") {
        return Some(("Click".into(), json!({ "loc": "Send" })));
    }
    None
}

pub(super) fn browser_extension_offline(history: &[(String, String)]) -> bool {
    history
        .iter()
        .any(|(_, o)| observation_indicates_extension_offline(o))
}

/// Skip slow llama planning for clear "open <site>" goals.
/// Compound goals (open + send/compose/…) may still bootstrap-navigate, but must NOT finalize here.
/// A live-data question (weather, markets, news, prices, scores) starts with a Google search for it.
/// Google puts the current values in a card at the top of the results, which is what the model can
/// read from the first part of the page; picking a site itself, the 8B model wandered into
/// finance home pages and gave up. Skipping that planning round also saves about a minute.
pub(super) fn scripted_live_search_action(
    goal: &str,
    step: u32,
    history: &[(String, String)],
    tools: &[McpTool],
) -> Option<(String, Value)> {
    let g = goal.to_ascii_lowercase();
    // With the search tool on offer the first step is a search through it (a numbered, dated list with addresses to cite), not a Google page in the
    // browser, which the search engine's terms discourage and which breaks on consent pages.
    if tools.iter().any(|t| t.name == "web.search")
        && step == 1
        && history.is_empty()
        && (crate::laya_validator::looks_live_info(goal) || crate::laya_validator::needs_web_lookup(goal))
        && !crate::agent_tools::asks_about_weather(goal)
        && !is_email_send_goal(goal)
        && !["notepad", "calculator", "desktop", "window", "email", "mail ", "send ", "open ", "launch "].iter().any(|w| g.contains(w))
    {
        let mut words: Vec<String> = significant_goal_tokens(goal);
        if !words.is_empty() {
            if crate::laya_validator::looks_live_info(goal) && !words.iter().any(|w| ["today", "todays", "now", "live", "latest", "current", "currently"].contains(&w.as_str())) {
                words.push("today".into());
            }
            return Some(("web.search".into(), json!({ "query": words.join(" "), "limit": 8 })));
        }
    }
    let lookup = crate::laya_validator::needs_web_lookup(goal)
        && !crate::laya_validator::looks_live_info(goal);
    if lookup {
        return scripted_research_action(goal, history, tools);
    }
    if step != 1 || !history.is_empty() {
        return None;
    }
    if !crate::laya_validator::looks_live_info(goal) || is_email_send_goal(goal) {
        return None;
    }
    // Goals that name a desktop app or an action are not searches.
    if ["notepad", "calculator", "desktop", "window", "email", "mail ", "send ", "open ", "launch "].iter().any(|w| g.contains(w)) {
        return None;
    }
    if !tools.iter().any(|t| t.name == "browser_navigate") || browser_extension_offline(history) {
        return None;
    }
    let mut words: Vec<String> = significant_goal_tokens(goal);
    if words.is_empty() {
        return None;
    }
    if crate::laya_validator::looks_live_info(goal)
        && !words.iter().any(|w| ["today", "todays", "now", "live", "latest", "current", "currently"].contains(&w.as_str()))
    {
        words.push("today".into());
    }
    let query = words.join(" ");
    Some((
        "browser_navigate".into(),
        json!({ "url": format!("https://www.google.com/search?q={}", url_encode(&query)) }),
    ))
}

/// A research question must not be sent to whatever page the browser already has open (Gmail, for example).
pub(super) fn navigation_for_goal(goal: &str, tool: &str, mut arguments: Value) -> Value {
    if !tool.eq_ignore_ascii_case("browser_navigate") && !tool.eq_ignore_ascii_case("local_open_url") {
        return arguments;
    }
    let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let g = goal.to_ascii_lowercase();
    let about_mail = ["mail", "gmail", "inbox", "email"].iter().any(|w| g.contains(w));
    let mail_page = url.contains("mail.google.com") || url.contains("google.com/mail");
    if mail_page && !about_mail {
        if let Some(search) = google_search_url(goal) {
            arguments["url"] = json!(search);
        }
    }
    arguments
}

fn google_search_url(goal: &str) -> Option<String> {
    let words = significant_goal_tokens(goal);
    if words.is_empty() {
        return None;
    }
    Some(format!("https://www.google.com/search?q={}", url_encode(&words.join(" "))))
}

/// After the Google search for a "what is X" question: read the results, open the site, then read that page and up to two more pages on the same site.
fn scripted_research_action(
    goal: &str,
    history: &[(String, String)],
    tools: &[McpTool],
) -> Option<(String, Value)> {
    if !tools.iter().any(|t| t.name == "browser_navigate") || browser_extension_offline(history) {
        return None;
    }
    let g = goal.to_ascii_lowercase();
    if ["notepad", "calculator", "desktop", "window", "email", "mail ", "send ", "open ", "launch "].iter().any(|w| g.contains(w)) {
        return None;
    }
    if history.is_empty() {
        let url = google_search_url(goal)?;
        return Some(("browser_navigate".into(), json!({ "url": url })));
    }
    let can_extract = tools.iter().any(|t| t.name == "aqua_browser_extract");
    let visited = visited_sites(history);
    if !history.iter().any(|(t, _)| t == "aqua_browser_extract") && can_extract {
        return Some(("aqua_browser_extract".into(), json!({ "kind": "search_results" })));
    }
    if visited.is_empty() {
        let blob = history.iter().map(|(_, o)| o.as_str()).collect::<Vec<_>>().join("\n");
        let url = best_result_url(goal, &blob)?;
        return Some(("browser_navigate".into(), json!({ "url": url })));
    }
    let last_tool = history.last().map(|(t, _)| t.as_str()).unwrap_or("");
    if last_tool == "browser_navigate" && can_extract {
        return Some(("aqua_browser_extract".into(), json!({ "kind": "article" })));
    }
    // One read of the home page, then up to two other pages on that site. Never the same address again.
    if visited.len() < 3 {
        if let Some(url) = next_site_page(goal, history) {
            return Some(("browser_navigate".into(), json!({ "url": url })));
        }
    }
    None
}

pub(super) fn visited_sites(history: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (tool, obs) in history {
        if tool != "browser_navigate" {
            continue;
        }
        let Some(url) = obs.lines().find_map(|l| {
            let t = l.trim();
            t.strip_prefix("url=")
                .or_else(|| t.strip_prefix("Page URL:"))
                .map(str::trim)
        }) else {
            continue;
        };
        if url.contains("google.com/search") || url.contains("google.com/mail") || url.contains("google.com/webhp") {
            continue;
        }
        let key = url_key(url);
        if !out.iter().any(|u| url_key(u) == key) {
            out.push(url.to_string());
        }
    }
    out
}

pub(super) fn url_key(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let no_hash = lower.split('#').next().unwrap_or(&lower);
    let no_query = no_hash.split('?').next().unwrap_or(no_hash);
    no_query.trim_end_matches('/').trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.").to_string()
}

fn best_result_url(goal: &str, text: &str) -> Option<String> {
    let tokens = significant_goal_tokens(goal);
    let mut urls = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(start) = rest.find("https://").or_else(|| rest.find("http://")) {
            let slice = &rest[start..];
            let raw = slice.split_whitespace().next().unwrap_or(slice);
            let raw = raw.trim_end_matches(|c: char| matches!(c, '.' | ',' | ')' | ']' | '"' | '—' | '-'));
            let url = unwrap_redirect(raw);
            if !url.is_empty() && !urls.iter().any(|u: &String| url_key(u) == url_key(&url)) {
                urls.push(url);
            }
            rest = &slice[5.min(slice.len())..];
        }
    }
    let usable = |u: &str| {
        let l = u.to_ascii_lowercase();
        !l.contains("google.") && !l.contains("gstatic.") && !l.contains("googleusercontent.") && !l.contains("youtube.")
    };
    urls.iter()
        .find(|u| usable(u) && tokens.iter().any(|t| url_key(u).contains(t.as_str())))
        .or_else(|| urls.iter().find(|u| usable(u)))
        .cloned()
}

fn unwrap_redirect(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.contains("google.") && (lower.contains("/url?") || lower.contains("/goto?") || lower.contains("url=") || lower.contains("&q=")) {
        for key in ["url=", "q="] {
            if let Some(v) = url.split(key).nth(1) {
                let decoded = percent_decode(v.split('&').next().unwrap_or(v));
                if decoded.starts_with("http://") || decoded.starts_with("https://") {
                    return decoded;
                }
            }
        }
    }
    url.to_string()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn site_host(url: &str) -> String {
    url_key(url).split('/').next().unwrap_or("").to_string()
}

fn next_site_page(goal: &str, history: &[(String, String)]) -> Option<String> {
    let visited = visited_sites(history);
    let host = site_host(visited.first()?);
    if host.is_empty() {
        return None;
    }
    let blob = history.iter().map(|(_, o)| o.as_str()).collect::<Vec<_>>().join("\n");
    let mut rest = blob.as_str();
    while let Some(start) = rest.find("https://").or_else(|| rest.find("http://")) {
        let slice = &rest[start..];
        let raw = slice.split_whitespace().next().unwrap_or(slice);
        let raw = raw.trim_end_matches(|c: char| matches!(c, '.' | ',' | ')' | ']' | '"' | '—' | '-'));
        let url = unwrap_redirect(raw);
        let key = url_key(&url);
        if site_host(&url) == host && key != host && !visited.iter().any(|seen| url_key(seen) == key) {
            return Some(url);
        }
        rest = &slice[5.min(slice.len())..];
    }
    let _ = goal;
    None
}

fn readable_page_text(obs: &str) -> Option<String> {
    if obs.contains("untrusted") || obs.contains("google.com/goto") || obs.contains("google.com/url?") {
        return None;
    }
    let link_lines = obs.lines().filter(|l| l.contains(" — http")).count();
    if link_lines >= 2 {
        return None;
    }
    let text = obs
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("url=")
                && !l.starts_with("Page URL:")
                && !l.starts_with("Status Code:")
                && !l.contains(" — http")
        })
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() < 40 {
        None
    } else {
        Some(text.chars().take(500).collect())
    }
}

/// Once a research question has been read on the site itself, say what those pages said.
pub(super) fn research_page_answer(goal: &str, history: &[(String, String)]) -> Option<String> {
    if !crate::laya_validator::needs_web_lookup(goal) || crate::laya_validator::looks_live_info(goal) {
        return None;
    }
    if visited_sites(history).is_empty() {
        return None;
    }
    if scripted_research_action(goal, history, &[
        McpTool { name: "browser_navigate".into(), description: String::new(), input_schema: json!({}), source: None },
        McpTool { name: "aqua_browser_extract".into(), description: String::new(), input_schema: json!({}), source: None },
    ]).is_some() {
        return None;
    }
    let mut pages = Vec::new();
    let mut pending_url = String::new();
    for (tool, obs) in history {
        if tool == "browser_navigate" {
            if let Some(url) = obs.lines().find_map(|l| l.trim().strip_prefix("url=")) {
                if !url.contains("google.") {
                    pending_url = url.to_string();
                }
            }
        }
        if tool == "aqua_browser_extract" {
            if let Some(body) = readable_page_text(obs) {
                pages.push(format!("{pending_url}: {body}"));
            }
        }
    }
    if pages.is_empty() {
        return None;
    }
    Some(format!("Regarding \"{goal}\":\n{}", pages.join("\n\n")))
}

pub(super) fn scripted_open_action(
    goal: &str,
    step: u32,
    history: &[(String, String)],
    tools: &[McpTool],
) -> Option<(String, Value)> {
    if step != 1 || !history.is_empty() {
        return None;
    }
    // Email-send goals use compose deep-link instead of opening the inbox alone.
    if is_email_send_goal(goal) {
        return None;
    }
    let g = goal.to_ascii_lowercase();
    let openish = g.contains("open ")
        || g.starts_with("open")
        || g.contains("launch ")
        || g.contains("go to ")
        || g.contains("visit ")
        || g.contains("navigate")
        || g.contains("for me");
    if !openish {
        return None;
    }
    let url = web_fallback_url(goal)?;
    let compound = goal_has_follow_on_work(goal);
    let mut args = json!({ "url": url });
    if let Some(browser) = requested_browser(goal) {
        args["browser"] = json!(browser);
    }
    // Multi-step work needs the Browser MCP session so Click/Type can continue on the page.
    if compound {
        if tools.iter().any(|t| t.name == "browser_navigate") && !browser_extension_offline(history)
        {
            return Some(("browser_navigate".into(), args));
        }
        // Still open the site as step 1; caller must continue the agent loop.
        return Some(("local_open_url".into(), args));
    }
    // Navigation-only: local start is enough.
    Some(("local_open_url".into(), args))
}

/// "in Brave open Facebook" names the browser. Absent means the default browser.
pub(super) fn requested_browser(goal: &str) -> Option<&'static str> {
    let words = goal_words(goal);
    const NAMED: &[(&str, &[&str])] = &[
        ("brave", &["brave"]),
        ("edge", &["edge", "microsoft edge"]),
        ("chrome", &["chrome", "google chrome"]),
        ("firefox", &["firefox"]),
    ];
    NAMED.iter().find_map(|(name, keys)| keys.iter().any(|k| words.contains(&format!(" {k} "))).then_some(*name))
}

fn browser_executable(name: &str) -> Option<std::path::PathBuf> {
    let rels: &[&str] = match name {
        "brave" => &[r"BraveSoftware\Brave-Browser\Application\brave.exe"],
        "edge" => &[r"Microsoft\Edge\Application\msedge.exe"],
        "chrome" => &[r"Google\Chrome\Application\chrome.exe"],
        "firefox" => &[r"Mozilla Firefox\firefox.exe"],
        _ => return None,
    };
    let bases = [
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from),
        std::env::var_os("ProgramFiles").map(std::path::PathBuf::from),
        std::env::var_os("ProgramFiles(x86)").map(std::path::PathBuf::from),
    ];
    bases.into_iter().flatten().flat_map(|base| rels.iter().map(move |rel| base.join(rel))).find(|p| p.is_file())
}

/// The app a plain "open Notepad" request names: one to three words after open/launch/start, nothing else in the
/// request. Websites, "the installer" and other things for the agent to work out are not plain app names.
pub(crate) fn plain_open_app_request(goal: &str) -> Option<String> {
    // "Hey Aqua, open Notepad." (with the comma speech recognition writes) is "open Notepad."
    let goal = &crate::voice_commands::without_wake_phrase(goal, "Hey Aqua");
    let mut g = goal.trim().trim_end_matches(['.', '!', '?']).trim().to_ascii_lowercase();
    for lead in ["hey aqua ", "aqua ", "please ", "can you ", "could you ", "would you ", "i want you to ", "i want to "] {
        if let Some(rest) = g.strip_prefix(lead) {
            g = rest.trim_start().to_string();
        }
    }
    let g = g.strip_suffix(" please").unwrap_or(&g).trim().to_string();
    let rest = ["open ", "launch ", "start "].iter().find_map(|v| g.strip_prefix(v))?.trim();
    let rest = rest.strip_prefix("the ").unwrap_or(rest);
    let rest = ["app", "application", "program"].iter().find_map(|s| rest.strip_suffix(&format!(" {s}"))).unwrap_or(rest).trim();
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.is_empty() || words.len() > 3 || !words.iter().all(|w| w.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')) {
        return None;
    }
    // "this" and "my" are blocked below so "open this file" stays with the agent. These two folders are real targets.
    if rest == "this pc" {
        return Some("This PC".into());
    }
    if rest == "my computer" {
        return Some("My Computer".into());
    }
    const NOT_AN_APP_NAME: &[&str] = &[
        "it", "that", "this", "them", "one", "file", "files", "folder", "folders", "executable", "exe", "installer", "setup",
        "shortcut", "game", "and", "then", "with", "for", "in", "on", "me", "my", "first", "second", "third", "last", "result", "results",
    ];
    if words.iter().any(|w| NOT_AN_APP_NAME.contains(w) || w.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let name = words.join(" ");
    // A site ("open youtube") is the web path's job.
    if web_fallback_url(goal).is_some() {
        return None;
    }
    Some(name.split(' ').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" "))
}

/// Start an app by name for a plain request, without a planning round of the language model (about a minute each on
/// a CPU). `aqua_mcp_adapters::app_launch` does it: a recipe learned from earlier launches, a program that ships with Windows,
/// or the file index, each a few milliseconds; the Windows-MCP helper (2 to 36 seconds, three PowerShell starts and a window
/// search) only when none of those could. It goes through the same Windows guard as the agent, for the same caller, so shells
/// and administration tools are still refused. None means "not a plain request": the agent handles it.
pub(crate) fn open_app_directly(goal: &str, windows: &Option<Arc<Mutex<StdioMcpClient>>>, caller: WindowsCaller) -> Option<String> {
    use aqua_mcp_adapters::app_launch::{launch, Options, Slow};
    let name = plain_open_app_request(goal)?;
    if name == "This PC" || name == "My Computer" {
        info!("open {name} directly (no planning round)");
        return Some(open_this_pc());
    }
    info!("open app directly: {name:?} (no planning round)");
    let learning = crate::memory::experience::service::service().settings();
    let opt = Options { learn: learning.learning_enabled, use_learned: learning.learning_enabled && learning.retrieval_enabled };
    let mut slow = |n: &str| -> Slow {
        let Some(client) = windows.clone() else { return Slow::Failed };
        if let Err(e) = ensure_started(&client, "Windows-MCP") {
            warn!("open {n}: Windows-MCP did not start: {e:#}");
            return Slow::Failed;
        }
        let obs = WindowsGuard::global().call(&client, None, "App", json!({ "mode": "launch", "name": n }), caller);
        if obs.contains("PERMISSION_DENIED") {
            return Slow::Denied(format!("I can't open {n}: it is a shell or administration tool, and I don't start those."));
        }
        let lower = obs.to_ascii_lowercase();
        // It may have started the app and then not answered: never start a second one on that.
        if lower.contains("did not answer") || lower.contains("timed out") || lower.contains("timeout") || lower.contains("backend_error") {
            warn!("open {n}: the Windows helper did not answer in time: {}", obs.chars().take(200).collect::<String>());
            return Slow::Unknown;
        }
        if observation_indicates_failure(&obs) {
            warn!("open {n} failed: {}", obs.chars().take(200).collect::<String>());
            return Slow::Failed;
        }
        Slow::Opened
    };
    Some(launch(&name, caller, &opt, &mut slow))
}

fn open_this_pc() -> String {
    match std::process::Command::new("explorer.exe").arg("shell:MyComputerFolder").spawn() {
        Ok(_) => "Opened This PC.".into(),
        Err(e) => {
            warn!("open This PC failed: {e}");
            format!("I could not open This PC: {e}")
        }
    }
}

pub(super) fn open_url_locally(url: &str, browser: Option<&str>) -> Result<String> {
    if let Some(name) = browser {
        // The URL is a command-line argument here, so it must be a plain http(s) address: Chromium would read `--gpu-launcher=...`
        // or `--renderer-cmd-prefix=...` as options and run the command in them. `open_web_url` makes the same check for the default browser.
        let url = url.trim();
        if !aqua_mcp_adapters::is_safe_web_url(url) {
            anyhow::bail!("refusing to open non-http(s) target in {name}: {url}");
        }
        let Some(exe) = browser_executable(name) else {
            anyhow::bail!("{name} is not installed");
        };
        let started = std::time::Instant::now();
        let spawned = std::process::Command::new(exe).arg(url).spawn();
        // Starting a browser goes round the tool host, so it writes its own ledger line (the address up to any query string).
        let shown: String = url.split(['?', '#']).next().unwrap_or("").chars().take(200).collect();
        aqua_tool_host::record_direct("direct", "shell.open_url", &shown, spawned.is_ok(), started.elapsed().as_millis() as u64);
        spawned.map_err(|e| anyhow::anyhow!("could not start {name}: {e}"))?;
        return Ok(format!("Opened {url} in {name}."));
    }
    // ShellExecuteW takes the URL as data: no shell parses it, so `&`, `^` or `%` cannot run a command.
    open_web_url(url)?;
    Ok(format!("Opened {url} in the default browser (local start)."))
}

/// What the planner is told after a failed call. An app that would not start gets the way round it (open the website); a browser call that did not find its element is told to
/// look at the page and use a ref from its snapshot and NOT to open the page again; any other tool gets the general words.
pub(super) fn failure_tail(goal: &str, tool: &str, obs: &str) -> String {
    if tool.eq_ignore_ascii_case("App") {
        if let Some(hint) = app_launch_fallback_hint(goal, obs) {
            return hint;
        }
    } else if tool.to_ascii_lowercase().starts_with("browser_") {
        let o = obs.to_ascii_lowercase();
        if o.contains("element_not_found") || o.contains("stale_ref") || o.contains("does not match any element") {
            return "That element is not on the page. Read the snapshot above and use the ref of the element you need (like e55) as the target. Do not open the page again. \
If the page is asking the person to sign in, or has no such place, say so in final. Output ONLY JSON tool call.".into();
        }
    }
    "Previous tool FAILED. Do NOT claim success. Try a different tool \
(browser_navigate for websites, or App with another name), \
then observe. Output ONLY JSON tool call.".into()
}

pub(super) fn app_launch_fallback_hint(goal: &str, obs: &str) -> Option<String> {
    if !observation_indicates_failure(obs) {
        return None;
    }
    let url = web_fallback_url(goal)?;
    Some(format!(
        "APP LAUNCH FAILED. Do NOT claim success. Next open the website directly \
(local_open_url / browser_navigate) with url \"{url}\". \
Then only final when the page evidences that site."
    ))
}

/// After Start-menu App fails for a known website, open the URL locally (skip another llama plan).
pub(super) fn try_web_fallback_after_app_fail(
    goal: &str,
    failed_tool: &str,
    steps: &mut Vec<AgentStep>,
    step: u32,
    max_steps: u32,
    progress_total: u32,
    tool_rounds: &mut u32,
    history: &mut Vec<(String, String)>,
    learning: &AgentLearning,
    progress: &Option<std::sync::mpsc::Sender<AgentStep>>,
    browser: &Option<BrowserHandle>,
) -> Option<McpAgentResult> {
    if !failed_tool.eq_ignore_ascii_case("App") {
        return None;
    }
    let url = web_fallback_url(goal)?;
    if let Err(why) = authorize_local_open(browser, url) {
        history.push(("local_open_url".to_string(), format!("ERROR [POLICY_BLOCKED_ORIGIN] local_open_url: {why}")));
        return None;
    }
    *tool_rounds += 1;
    let navigation_only = goal_is_navigation_only(goal);
    let emit = |steps: &mut Vec<AgentStep>, s: AgentStep| {
        if let Some(tx) = progress {
            let _ = tx.send(s.clone());
        }
        steps.push(s);
    };
    emit(
        steps,
        phase_event(
            step,
            "act",
            "Act: open website (fallback)",
            "running",
            format!("App missed — opening {url} in default browser…"),
            Some("Finalize"),
            Some(step.saturating_add(2)),
            Some(progress_total),
        ),
    );
    emit(
        steps,
        AgentStep {
            step,
            kind: "tool_start".into(),
            detail: json!({
                "id": format!("tool-{step}-local_open_url"),
                "phase": "act",
                "title": "Call local_open_url",
                "status": "running",
                "tool": "local_open_url",
                "arguments": { "url": url },
                "message": format!("Round {step}/{max_steps}: fallback open {url}"),
                "next": "Finalize",
                "progress": { "current": step.saturating_add(2), "total": progress_total },
            }),
        },
    );
    match open_url_locally(url, None) {
        Ok(obs) => {
            let next = if navigation_only {
                "Finalize"
            } else {
                "Continue remaining goal steps"
            };
            emit(
                steps,
                phase_event(
                    step,
                    "act",
                    "Act: open website (fallback)",
                    "completed",
                    format!("Opened {url}"),
                    Some(next),
                    Some(step.saturating_add(2)),
                    Some(progress_total),
                ),
            );
            emit(
                steps,
                AgentStep {
                    step,
                    kind: "tool".into(),
                    detail: json!({
                        "id": format!("tool-{step}-local_open_url"),
                        "phase": "act",
                        "title": "local_open_url ok",
                        "status": "completed",
                        "tool": "local_open_url",
                        "source": "local",
                        "arguments": { "url": url },
                        "observation": record_observation(&obs),
                        "message": format!("✅ Opened {url} after App miss."),
                        "next": next,
                    }),
                },
            );
            history.push(("local_open_url".into(), obs));
            // Compound goals (send/compose/…) must keep the agent loop running.
            if !navigation_only {
                return None;
            }
            let final_answer = format!("Opened {url} in your default browser.");
            emit(
                steps,
                phase_event(
                    step,
                    "finalize",
                    "Finalize answer",
                    "completed",
                    format!("Opened {url} (App miss → web fallback)"),
                    None,
                    Some(progress_total),
                    Some(progress_total),
                ),
            );
            emit(
                steps,
                AgentStep {
                    step,
                    kind: "final".into(),
                    detail: json!({
                        "id": format!("final-{step}"),
                        "phase": "finalize",
                        "title": "Task complete",
                        "status": "completed",
                        "answer": final_answer,
                        "message": "Recovered via website fallback.",
                        "progress": { "current": progress_total, "total": progress_total },
                    }),
                },
            );
            let result = McpAgentResult {
                final_answer,
                steps: std::mem::take(steps),
                tool_rounds: *tool_rounds,
                stopped_reason: "app_fail_web_fallback".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            };
            learning.learn_success(
                goal,
                &recipe_from_steps(&result.steps, &result.final_answer),
            );
            Some(result)
        }
        Err(e) => {
            emit(
                steps,
                phase_event(
                    step,
                    "act",
                    "Act: open website (fallback)",
                    "failed",
                    format!("Fallback open failed: {e:#}"),
                    Some("Replan"),
                    Some(step.saturating_add(2)),
                    Some(progress_total),
                ),
            );
            history.push((
                "local_open_url".into(),
                format!("ERROR opening {url}: {e:#}"),
            ));
            None
        }
    }
}

/// Text that is a snapshot of the desktop or of a page rather than prose: window lists, UI trees,
/// element handles.
pub(super) fn goal_is_news(goal: &str) -> bool {
    let g = goal.to_ascii_lowercase();
    g.contains("news") || g.contains("headline")
}

/// Quoted titles on a page snapshot, skipping the desktop-window list and search-page chrome. At most ten.
pub(super) fn headline_lines(text: &str) -> Vec<String> {
    let mut titles: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.contains("Opened Windows") || t.contains("Focused Window") || t.contains("Cursor Position") || t.contains("UI Tree") {
            continue;
        }
        let mut rest = t;
        while let Some(start) = rest.find('"') {
            rest = &rest[start + 1..];
            let Some(end) = rest.find('"') else { break };
            let raw = rest[..end].trim();
            rest = &rest[end + 1..];
            let Some(title) = clean_headline(raw) else { continue };
            if let Some(existing) = titles.iter_mut().find(|e| same_story(e, &title)) {
                if cleaner_headline(&title, existing) {
                    *existing = title;
                }
            } else if titles.len() < 10 {
                titles.push(title);
            }
            if titles.len() == 10 {
                break;
            }
        }
    }
    titles.into_iter().enumerate().map(|(i, t)| format!("{}. {t}", i + 1)).collect()
}

/// Drop search-page labels, video controls and ages. `None` when nothing newsworthy is left.
fn clean_headline(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("play on google") || lower.starts_with("video.") || lower.trim() == "video" {
        return None;
    }
    let mut s = raw.trim().to_string();
    if let Some(i) = s.to_ascii_lowercase().find("about this result") {
        s = s[i + "about this result".len()..].trim().to_string();
    }
    // A leading clock such as "7:09 " or "1:25 " is a video length, not part of the title.
    if let Some(rest) = s.split_once(' ') {
        let clock = rest.0;
        if clock.len() <= 5 && clock.contains(':') && clock.chars().all(|c| c.is_ascii_digit() || c == ':') {
            s = rest.1.trim().to_string();
        }
    }
    for tail in [" hours ago", " hour ago", " days ago", " day ago", " minutes ago", " minute ago"] {
        if let Some(i) = s.to_ascii_lowercase().rfind(tail) {
            let before = s[..i].trim_end();
            let age = before.rsplit_once(' ').map(|(_, n)| n).unwrap_or("");
            if age.chars().all(|c| c.is_ascii_digit()) {
                s = before[..before.len() - age.len()].trim().to_string();
            }
        }
    }
    if s.to_ascii_lowercase().ends_with(" video") {
        s = s[..s.len() - " video".len()].trim().to_string();
    }
    if let Some(i) = s.find("\" :") {
        s = s[..i].trim().to_string();
    }
    s = s.trim_matches('"').trim().trim_end_matches(':').trim().to_string();
    s = s.trim_end_matches("...").trim().to_string();
    if !english_headline(&s) || site_banner(&s) || search_prompt_not_story(&s) {
        return None;
    }
    let n = s.chars().count();
    if !(20..=160).contains(&n) || s.to_ascii_lowercase().contains("about this result") {
        return None;
    }
    Some(s)
}

/// A Google "related search" asks for the news. It does not report an event.
fn search_prompt_not_story(title: &str) -> bool {
    let l = title.to_ascii_lowercase();
    let asks = l.ends_with('?')
        || l.starts_with("what ")
        || l.starts_with("how ")
        || l.starts_with("why ")
        || l.starts_with("when ")
        || l.starts_with("where ")
        || l.starts_with("which ");
    if !asks {
        return false;
    }
    l.contains("headline")
        || l.contains("top 10")
        || l.contains("top ten")
        || (l.contains("news") && (l.contains("today") || l.contains("latest")))
}

pub(super) fn observation_is_search_page(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    l.contains("google.com/search") || l.contains("about this result") || l.contains("people also ask")
}

fn site_banner(title: &str) -> bool {
    let l = title.to_ascii_lowercase();
    let marks = ["breaking news", "latest news", "get latest news", "india news"];
    marks.iter().filter(|m| l.contains(*m)).count() >= 2
}

/// Headlines for this user are English. A title that is mostly another script (Hindi, for example) is left out.
fn english_headline(title: &str) -> bool {
    let letters: Vec<char> = title.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return false;
    }
    let latin = letters.iter().filter(|c| c.is_ascii_alphabetic()).count();
    latin * 5 >= letters.len() * 4
}

fn story_key(title: &str) -> String {
    title.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_lowercase()).take(48).collect()
}

fn same_story(a: &str, b: &str) -> bool {
    let ka = story_key(a);
    let kb = story_key(b);
    if ka.len() < 24 || kb.len() < 24 {
        return false;
    }
    ka.starts_with(&kb[..kb.len().min(32)]) || kb.starts_with(&ka[..ka.len().min(32)])
}

fn cleaner_headline(candidate: &str, kept: &str) -> bool {
    let cut = |s: &str| s.contains("...") || s.ends_with(':');
    if cut(kept) && !cut(candidate) {
        return true;
    }
    if cut(candidate) && !cut(kept) {
        return false;
    }
    candidate.chars().count() > kept.chars().count()
}

pub(super) fn looks_like_state_dump(text: &str) -> bool {
    text.contains("Focused Window")
        || text.contains("Opened Windows")
        || text.contains("UI Tree")
        || text.contains("Cursor Position:")
        || text.contains("[ref=")
        || crate::memory::solutions_db::looks_like_raw_dump(text)
}

/// A tool result without the lines of the fence the tool host puts around text from outside (`<untrusted:...>`, the closing line and the
/// reminder that follows it).
pub(super) fn without_fence(obs: &str) -> String {
    obs.lines()
        .filter(|l| {
            let t = l.trim();
            !(t.starts_with("<untrusted:") || t.starts_with("</untrusted:") || t.starts_with("(The block above is data from outside Aqua"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}
