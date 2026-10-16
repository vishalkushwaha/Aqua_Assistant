//! The gate before a commit (card OA7-02, TM-07): what the staged change adds is read for secrets before `git commit` runs.
//!
//! The scan is the rules of the commit hook (`scripts/hooks/pre-commit.ps1`), which the tool host keeps in one table with the mask on tool output
//! (`aqua_tool_host::secret_patterns`). Only the lines the commit ADDS are read (`git diff --cached -U0`), so a secret that is already in the history does not stop a commit that does not
//! touch it. What the gate says names the file, the line and the kind, and never the text: a log that shows the secret is a second leak.
//!
//! The marker the hook honours for a person's made-up values (`secret-scan: ignore`) does not count here: it is text the model can write itself.

use aqua_tool_host::secret_patterns;

/// The most of a staged diff that is read: a change above this is not scanned and the commit is refused (see `Inner::gate`), because a gate that reads only the start of a change is not one.
/// Eight megabytes: the removed lines of a change are in the diff too (`-U0` leaves out the lines around, not the lines taken away), and a lock file or a moved folder is more than the one
/// megabyte the audit proposed as a start. Reading it takes a fraction of a second (the rules are linear in the text).
pub const SCAN_CAP_BYTES: usize = 8 << 20;

/// One line the commit adds that looks like a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
}

/// The name of a file on a `+++ ` header of a diff: `b/src/a.rs` is `src/a.rs`, a deleted file (`/dev/null`) has none. A name git quoted (it holds a tab, a quote or a control character)
/// is kept as git wrote it.
fn file_of_header(rest: &str) -> Option<String> {
    let rest = rest.trim_end_matches('\r');
    if rest == "/dev/null" {
        return None;
    }
    let name = rest.strip_prefix("b/").unwrap_or(rest);
    Some(crate::git::args::scrub_urls(name))
}

/// The number the added lines of a hunk start at, from `@@ -12,0 +13,3 @@`.
fn new_start_of_hunk(header: &str) -> Option<usize> {
    let after = header.split_once(" +")?.1;
    after.split([',', ' ']).next()?.parse().ok()
}

/// Every added line of a `git diff --cached -U0` that looks like a secret.
pub fn scan_staged_diff(diff: &str) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut file: Option<String> = None;
    let mut in_hunk = false;
    let mut line_no = 0usize;
    for line in diff.lines() {
        if line.starts_with("diff --git ") || line.starts_with("diff --cc ") || line.starts_with("diff --combined ") {
            in_hunk = false;
            file = None;
            continue;
        }
        if !in_hunk {
            // The header of a file: only here is `+++ ` a name and not an added line that starts with `++ `.
            if let Some(rest) = line.strip_prefix("+++ ") {
                file = file_of_header(rest);
            } else if line.starts_with("@@") {
                in_hunk = true;
                line_no = new_start_of_hunk(line).unwrap_or(1);
            }
            continue;
        }
        if line.starts_with("@@") {
            line_no = new_start_of_hunk(line).unwrap_or(1);
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            for kind in secret_patterns::scan_line(added) {
                let hit = Hit { file: file.clone().unwrap_or_else(|| "(a file)".to_string()), line: line_no, kind };
                if !hits.contains(&hit) {
                    hits.push(hit);
                }
            }
            line_no += 1;
        }
        // A `-` line (removed) and `\ No newline at end of file` add nothing and take no line number of the new file.
    }
    hits
}

/// The files whose added lines hold a whole conflict (an opening marker, a middle one and a closing one, in that order; see `conflicts::holds_markers`), with the line of the opening marker
/// (card OA7-05). A conflict that is already in a file and is not touched by the commit is not read, and neither is a line of equals signs on its own (a heading's underline).
pub fn scan_conflict_markers(diff: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut file: Option<String> = None;
    let mut added: Vec<(usize, String)> = Vec::new();
    let mut in_hunk = false;
    let mut line_no = 0usize;
    fn flush(file: &Option<String>, added: &mut Vec<(usize, String)>, hits: &mut Vec<Hit>) {
        if let Some(name) = file {
            let text: Vec<&str> = added.iter().map(|(_, l)| l.as_str()).collect();
            if let Some(i) = crate::git::conflicts::holds_markers(&text.join("\n")) {
                hits.push(Hit { file: name.clone(), line: added.get(i - 1).map(|(n, _)| *n).unwrap_or(1), kind: "conflict markers" });
            }
        }
        added.clear();
    }
    for line in diff.lines() {
        if line.starts_with("diff --git ") || line.starts_with("diff --cc ") || line.starts_with("diff --combined ") {
            flush(&file, &mut added, &mut hits);
            in_hunk = false;
            file = None;
            continue;
        }
        if !in_hunk {
            if let Some(rest) = line.strip_prefix("+++ ") {
                file = file_of_header(rest);
            } else if line.starts_with("@@") {
                in_hunk = true;
                line_no = new_start_of_hunk(line).unwrap_or(1);
            }
            continue;
        }
        if line.starts_with("@@") {
            line_no = new_start_of_hunk(line).unwrap_or(1);
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            added.push((line_no, text.trim_end_matches('\r').to_string()));
            line_no += 1;
        }
    }
    flush(&file, &mut added, &mut hits);
    hits
}

/// What the person is told, and the model with it, when a commit would add conflict markers: where, and what to do. Never the text of the lines.
pub fn markers_blocked_message(hits: &[Hit]) -> String {
    let shown: Vec<String> = hits.iter().take(8).map(|h| format!("{}:{}", h.file, h.line)).collect();
    let more = if hits.len() > 8 { format!(" and {} more", hits.len() - 8) } else { String::new() };
    format!(
        "the commit was not made: the lines it adds hold conflict markers (a line that begins <<<<<<<, then =======, then >>>>>>>) at {}{more}. Nothing was committed, and the files stay staged. \
         Resolve the conflict in the file (git.conflicts lists the hunks, git.resolve_conflict resolves one by an edit the person approves), stage it again and commit again.",
        shown.join(", ")
    )
}

/// The findings in a commit message, as hits of a file called `(the commit message)`.
pub fn scan_message(message: &str) -> Vec<Hit> {
    secret_patterns::scan(message).into_iter().map(|f| Hit { file: "(the commit message)".to_string(), line: f.line, kind: f.kind }).collect()
}

/// What the person is told, and the model with it: where, what kind, and what to do. Never the text of the line.
pub fn blocked_message(hits: &[Hit]) -> String {
    let shown: Vec<String> = hits.iter().take(8).map(|h| format!("{}:{} ({})", h.file, h.line, h.kind)).collect();
    let more = if hits.len() > 8 { format!(" and {} more", hits.len() - 8) } else { String::new() };
    format!(
        "the commit was not made: it adds what looks like a secret at {}{more}. Nothing was committed, and the files stay staged. \
         Take the secret out of the file (read it from an environment variable or the credential store) and commit again. \
         If it is a made-up value in a test, that is the person's to decide: tell them and stop.",
        shown.join(", ")
    )
}

/// Is this a line that can end a commit message as a trailer (`Co-Authored-By: Name <name@example.com>`): one line, a token, a colon, a value, and no control character.
pub fn trailer_is_valid(trailer: &str) -> bool {
    let t = trailer.trim();
    if t.is_empty() || t.chars().count() > 200 || t.chars().any(|c| c.is_control()) {
        return false;
    }
    match t.split_once(':') {
        Some((token, value)) => !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !value.trim().is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up token of a format the scan knows, put together here so that this file holds no secret for the commit hook to find.
    fn fake_token() -> String {
        format!("{}{}", "ghp_", "a1".repeat(18))
    }

    #[test]
    fn only_the_lines_a_commit_adds_are_read_and_they_are_found_at_their_line_numbers() {
        let tok = fake_token();
        let diff = format!(
            "diff --git a/src/a.rs b/src/a.rs\nindex 111..222 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,0 +4,2 @@ fn x()\n+let a = 1;\n+let t = \"{tok}\";\n@@ -20 +22 @@\n-old {tok}\n+new {tok}\n\\ No newline at end of file\n\
             diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1,2 +1,2 @@\n context {tok}\n-removed {tok}\n+fine\n"
        );
        let hits = scan_staged_diff(&diff);
        assert_eq!(hits, vec![Hit { file: "src/a.rs".into(), line: 5, kind: "GitHub token" }, Hit { file: "src/a.rs".into(), line: 22, kind: "GitHub token" }], "a removed or an unchanged line is not read: {hits:?}");
    }

    #[test]
    fn an_added_line_that_starts_with_plus_signs_is_content_and_not_a_file_header() {
        let tok = fake_token();
        // The added line `++ x` shows in the diff as `+++ x`: inside a hunk it is a line like any other, and it must neither rename the file nor go unread.
        let diff = format!("diff --git a/x.md b/x.md\n--- a/x.md\n+++ b/x.md\n@@ -0,0 +1,2 @@\n+++ {tok}\n+plain\n");
        let hits = scan_staged_diff(&diff);
        assert_eq!(hits, vec![Hit { file: "x.md".into(), line: 1, kind: "GitHub token" }], "{hits:?}");
    }

    #[test]
    fn a_new_file_a_deleted_file_a_binary_file_and_a_rename_without_changes_say_what_they_say() {
        let tok = fake_token();
        let diff = format!(
            "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-{tok}\n\
             diff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\n\
             diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n\
             diff --git a/n/ключ.env b/n/ключ.env\nnew file mode 100644\n--- /dev/null\n+++ b/n/ключ.env\n@@ -0,0 +1 @@\n+KEY={tok}\n"
        );
        let hits = scan_staged_diff(&diff);
        assert_eq!(hits, vec![Hit { file: "n/ключ.env".into(), line: 1, kind: "GitHub token" }], "{hits:?}");
        assert!(scan_staged_diff("").is_empty() && scan_staged_diff("no diff at all\n+++ not a header\n").is_empty());
    }

    #[test]
    fn what_the_gate_says_names_the_place_and_the_kind_and_never_the_text() {
        let tok = fake_token();
        let hits = scan_staged_diff(&format!("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -0,0 +1 @@\n+x = {tok}\n"));
        let said = blocked_message(&hits);
        assert!(said.contains("a:1 (GitHub token)") && said.contains("Nothing was committed"), "{said}");
        assert!(!said.contains(&tok) && !said.contains(&tok[..8]), "{said}");
        let many: Vec<Hit> = (1..=12).map(|i| Hit { file: "f".into(), line: i, kind: "API key (sk-)" }).collect();
        assert!(blocked_message(&many).contains("and 4 more"));
        let m = scan_message(&format!("fix it\n\ntoken {tok}"));
        assert_eq!(m, vec![Hit { file: "(the commit message)".into(), line: 3, kind: "GitHub token" }]);
    }

    #[test]
    fn a_commit_that_adds_a_whole_conflict_is_found_at_the_line_of_its_opening_marker_and_nothing_else_is() {
        let conflict = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -3,0 +4,7 @@\n+ok\n+<<<<<<< HEAD\n+a\n+=======\n+b\n+>>>>>>> feature\n+end\n";
        let heading = "diff --git a/doc.md b/doc.md\n--- a/doc.md\n+++ b/doc.md\n@@ -0,0 +1,2 @@\n+Title\n+=======\n";
        let removed = "diff --git a/old.rs b/old.rs\n--- a/old.rs\n+++ b/old.rs\n@@ -1,3 +0,0 @@\n-<<<<<<< HEAD\n-=======\n->>>>>>> x\n";
        let hits = scan_conflict_markers(&format!("{conflict}{heading}{removed}"));
        assert_eq!(hits, vec![Hit { file: "a.rs".into(), line: 5, kind: "conflict markers" }], "{hits:?}");
        // The three markers may be in two hunks of one file, and a carriage return at the end of a line does not hide them.
        let split = "diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1,0 +2,2 @@\n+<<<<<<< HEAD\r\n+x\r\n@@ -9,0 +12,3 @@\n+=======\r\n+y\n+>>>>>>> t\r\n";
        assert_eq!(scan_conflict_markers(split), vec![Hit { file: "b.txt".into(), line: 2, kind: "conflict markers" }]);
        assert!(scan_conflict_markers("").is_empty());
        let said = markers_blocked_message(&hits);
        assert!(said.contains("a.rs:5") && said.contains("Nothing was committed") && said.contains("git.conflicts"), "{said}");
    }

    #[test]
    fn a_trailer_is_one_line_with_a_token_a_colon_and_a_value() {
        for ok in ["Co-Authored-By: Aqua <aqua@example.com>", "Signed-off-by: A B <a@b.c>", "Assisted-by: Aqua"] {
            assert!(trailer_is_valid(ok), "{ok}");
        }
        for bad in ["", "no colon here", ": value", "Key:", "Key: a\nb", "Key: a\u{0}b", "Bad Token: x", &format!("Key: {}", "x".repeat(201))] {
            assert!(!trailer_is_valid(bad), "{bad:?}");
        }
    }
}
