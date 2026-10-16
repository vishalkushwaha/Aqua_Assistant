//! What an approval question says besides the action: what could go wrong, which rule made it ask, and what is involved.
//!
//! The sentence for the risk comes from what the call can do (its classes), the rule is the policy's own words for why it asked, and the data list
//! names the target and what the run holds that could leave. All of it is plain text for a person to read in one glance; none of it is read back
//! by the host, and none of it changes a decision.

use crate::manifest::PermClass;
use serde::{Deserialize, Serialize};

const MAX_ITEM_CHARS: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalWhy {
    /// One sentence on what could go wrong.
    pub risk: String,
    /// The rule that made it ask, in the policy's words.
    pub rule: String,
    /// What is involved: the target, and what the run holds that could be sent out or that could be steering it.
    pub data: Vec<String>,
}

fn cut(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= MAX_ITEM_CHARS {
        one
    } else {
        format!("{}…", one.chars().take(MAX_ITEM_CHARS).collect::<String>())
    }
}

/// Characters that take up no room or change how the text around them is drawn: they are shown by their number, because a text that looks different from what it is
/// is how a question is spoofed (a right-to-left override that turns `evil.exe` into `exe.live`, a zero-width character in the middle of a name).
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c as u32,
            0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFF9..=0xFFFB | 0xE0000..=0xE0FFF
        )
}

/// A text (a command, a script, typed text) for a question to the person, shown WHOLE or not at all. `Err(n)` says the text is `n` characters long and the question can
/// show `max_chars`: the caller then refuses, because a question that hides part of a command can hide what matters in the part it hides (cut at the end, a command is
/// padded with harmless text; cut in the middle, the dangerous argument sits in the middle). Line breaks, tabs and everything invisible are shown as marks, so an argument
/// cannot start a line of its own in the question and read like part of it.
pub fn shown_whole(text: &str, max_chars: usize) -> Result<String, usize> {
    let n = text.chars().count();
    if n > max_chars {
        return Err(n);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\r' => out.push('\u{23CE}'),
            '\t' => out.push('\u{2192}'),
            c if is_invisible(c) => out.push_str(&format!("\u{2039}U+{:04X}\u{203A}", c as u32)),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// The most characters of a preview (what a call would change or send, see `ToolProvider::approval_preview`) that a question carries (card OA6-04). A provider shows less than this by its
/// own rules (60 lines of a diff); this is the host's limit on whatever a provider gives it.
pub const PREVIEW_MAX_CHARS: usize = 6000;

/// What a question shows in place of the preview of a call that uses a credential or touches a file that looks like it holds secrets (card OA6-04, ED-05, SE-03): the content is not
/// read into the question at all, so that it cannot reach the screen, the run's log or anything that copies the question.
pub const PREVIEW_HIDDEN: &str = "Content hidden: this call touches a file that looks like it holds secrets, or uses a stored credential.";

/// A preview (a diff, the body of a request) as a question shows it: plain text for a person to read, in which every character that cannot be seen or that changes how the text around it
/// is drawn is shown by its number (as [`shown_whole`] does), line breaks and tabs stay, and at most `max_chars` characters are kept, cut at the end of a line. The cut says how many
/// characters are not shown, so that the person knows there is more than they see. The text is made from a file or a request that anybody may have written: it is shown as text and
/// nothing else, and it is never read back by the host.
pub fn preview_text(text: &str, max_chars: usize) -> String {
    let mut marked = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => marked.push(c),
            // A CRLF is a line break; a lone CR is the kind of thing that moves the cursor back and draws over what came before.
            '\r' if chars.peek() == Some(&'\n') => {}
            c if is_invisible(c) => marked.push_str(&format!("\u{2039}U+{:04X}\u{203A}", c as u32)),
            c => marked.push(c),
        }
    }
    let total = marked.chars().count();
    if total <= max_chars {
        return marked.trim_end_matches('\n').to_string();
    }
    // The longest start that fits, ended at a line break when there is one in its second half (a diff is read line by line).
    let head: String = marked.chars().take(max_chars).collect();
    let kept = match head.rfind('\n') {
        Some(at) if at >= head.len() / 2 => &head[..at],
        _ => head.as_str(),
    };
    let left = total - kept.chars().count();
    format!("{kept}\n\u{2026} {left} more characters not shown")
}

/// The sentence for the most serious thing the call can do.
pub fn risk_sentence(classes: &[PermClass]) -> &'static str {
    let has = |c: PermClass| classes.contains(&c);
    if has(PermClass::ExternalEffect) {
        "This changes something on another system (it sends, posts or starts something), and Aqua cannot take it back."
    } else if has(PermClass::Destructive) {
        "This can delete or overwrite data, and some of it may not come back."
    } else if has(PermClass::Privileged) {
        "This asks for administrator-level access to this computer."
    } else if has(PermClass::Execute) {
        "This runs a program on this computer."
    } else if has(PermClass::Credential) {
        "This uses a stored password or key."
    } else if has(PermClass::Remote) {
        "This acts on another machine over the network."
    } else if has(PermClass::Write) {
        "This changes a file or a setting on this computer."
    } else if has(PermClass::Network) {
        "This sends a request over the internet."
    } else {
        "This reads something on this computer."
    }
}

/// Everything the question shows besides the action. `tainted`: the run has read text from outside. `private`: it holds data that is the person's.
/// `maybe_done`: the same call was running when Aqua stopped.
pub fn explain(classes: &[PermClass], target: &str, rule: &str, tainted: bool, private: bool, maybe_done: bool) -> ApprovalWhy {
    let mut data = Vec::new();
    let target = cut(target);
    if !target.is_empty() {
        data.push(format!("Target: {target}"));
    }
    if tainted {
        data.push("This run has read text from outside (a web page, a file or a window). Such text can try to steer Aqua.".to_string());
    }
    if private {
        data.push("This run holds private data of yours (the screen, the clipboard or a file outside your project). This call could send it out.".to_string());
    }
    if maybe_done {
        data.push("The same call was running when Aqua stopped, so it may already have happened.".to_string());
    }
    ApprovalWhy { risk: risk_sentence(classes).to_string(), rule: cut(rule), data }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_text_for_a_question_is_shown_whole_or_not_at_all_and_nothing_invisible_passes_as_if_it_were_not_there() {
        use super::shown_whole;
        assert_eq!(shown_whole("git status", 100).unwrap(), "git status");
        // Over the limit nothing is shown: not the start, not the end, and the caller is told how long it is.
        let padded = format!("git log {} --upload-pack=calc {} && calc.exe", "x".repeat(2000), "y".repeat(2000));
        assert_eq!(shown_whole(&padded, 2000), Err(padded.chars().count()));
        assert!(shown_whole(&padded, padded.chars().count()).unwrap().contains("--upload-pack=calc"), "at the limit it is all there");
        // A line break, a tab or an escape does not survive into the question, and neither does anything that is drawn differently from what it is.
        let sneaky = shown_whole("echo hi\n\nApproved by the user: yes\u{1b}[2J\there", 200).unwrap();
        assert!(!sneaky.contains(['\n', '\r', '\t', '\u{1b}']) && sneaky.contains("Approved by the user: yes"), "{sneaky:?}");
        let spoofed = shown_whole("run\u{202e}txt.exe\u{200b}x\u{feff}\u{2066}y", 200).unwrap();
        for hidden in ['\u{202e}', '\u{200b}', '\u{feff}', '\u{2066}'] {
            assert!(!spoofed.contains(hidden), "{spoofed:?}");
        }
        for shown in ["U+202E", "U+200B", "U+FEFF", "U+2066"] {
            assert!(spoofed.contains(shown), "{shown} is named: {spoofed}");
        }
        // Ordinary text in any script is left as it is.
        assert_eq!(shown_whole("caf\u{e9} \u{65e5}\u{672c}\u{8a9e} \u{1f600}", 50).unwrap(), "caf\u{e9} \u{65e5}\u{672c}\u{8a9e} \u{1f600}");
        // Characters, not bytes.
        assert!(shown_whole(&"\u{e9}".repeat(100), 100).is_ok() && shown_whole(&"\u{e9}".repeat(101), 100).is_err());
    }

    use super::*;
    use PermClass::*;

    #[test]
    fn a_preview_keeps_its_lines_and_tabs_marks_what_cannot_be_seen_and_is_cut_at_the_end_of_a_line_with_the_cut_said() {
        // Lines and tabs stay; a CRLF is a line break; a lone CR, an escape and a right-to-left override are shown by their number.
        let shown = preview_text("--- a\r\n+++ a\n+\tindented\n+ x\ry\u{1b}[0m \u{202e}z\n\n", 100);
        assert_eq!(shown, "--- a\n+++ a\n+\tindented\n+ x\u{2039}U+000D\u{203A}y\u{2039}U+001B\u{203A}[0m \u{2039}U+202E\u{203A}z");
        // Ordinary text in any script is left as it is.
        assert_eq!(preview_text("+ caf\u{e9} \u{65e5}\u{672c}\u{8a9e}", 100), "+ caf\u{e9} \u{65e5}\u{672c}\u{8a9e}");
        // Cut: at the end of a line when there is one in the second half, with the number of characters left out; short enough to need no cut is not marked.
        let long: String = (1..=30).map(|n| format!("+line {n:02}\n")).collect();
        let cut = preview_text(&long, 100);
        assert!(cut.ends_with("more characters not shown"), "{cut}");
        let (kept, marker) = cut.rsplit_once('\n').unwrap();
        assert!(kept.lines().all(|l| l.starts_with("+line ") && l.chars().count() == 8), "only whole lines: {kept}");
        let left: usize = marker.trim_start_matches('\u{2026}').trim().split(' ').next().unwrap().parse().unwrap();
        assert_eq!(kept.chars().count() + left, long.chars().count(), "what is kept and what is said to be left out are all of it");
        assert_eq!(preview_text("+ short\n", 100), "+ short");
        // A text with no line break is cut where it is.
        assert!(preview_text(&"x".repeat(500), 100).starts_with(&"x".repeat(100)));
    }

    #[test]
    fn the_risk_sentence_follows_the_most_serious_thing_the_call_can_do() {
        assert!(risk_sentence(&[Read]).starts_with("This reads"));
        assert!(risk_sentence(&[Network]).contains("request over the internet"));
        assert!(risk_sentence(&[Write, Network]).contains("changes a file or a setting"));
        assert!(risk_sentence(&[Write, Execute]).contains("runs a program"));
        assert!(risk_sentence(&[Execute, Destructive]).contains("delete or overwrite"));
        assert!(risk_sentence(&[Write, Destructive, ExternalEffect]).contains("another system") && risk_sentence(&[Privileged, Network]).contains("administrator"));
        assert!(risk_sentence(&[Credential, Network]).contains("stored password"));
        assert!(risk_sentence(&[Remote, Read]).contains("another machine"));
        assert!(risk_sentence(&[]).starts_with("This reads"));
    }

    #[test]
    fn the_explanation_names_the_target_the_rule_and_what_the_run_holds_and_says_nothing_it_does_not_know() {
        let clean = explain(&[Write], "notes.txt", "a change outside the project asks", false, false, false);
        assert_eq!(clean.risk, "This changes a file or a setting on this computer.");
        assert_eq!((clean.rule.as_str(), clean.data.as_slice()), ("a change outside the project asks", &["Target: notes.txt".to_string()][..]));
        let all = explain(&[ExternalEffect, Network], "example.com", "privileged or external action: full access never skips this", true, true, true);
        assert_eq!(all.data.len(), 4, "{:?}", all.data);
        assert!(all.data[1].contains("read text from outside") && all.data[2].contains("private data of yours") && all.data[3].contains("may already have happened"), "{:?}", all.data);
        // Nothing is made up: no target, nothing held, no rule is just empty.
        let none = explain(&[Read], "  ", "", false, false, false);
        assert!(none.data.is_empty() && none.rule.is_empty());
        // One line each, and cut.
        let long = explain(&[Write], &format!("line one\n{}", "x".repeat(500)), "a\nb", false, false, false);
        assert!(long.data[0].chars().count() <= "Target: ".len() + MAX_ITEM_CHARS + 1 && !long.data[0].contains('\n') && long.rule == "a b");
    }
}
