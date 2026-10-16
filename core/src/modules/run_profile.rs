//! One run profile instead of two keyword classifiers (card OA4-01; tasks AG-02, WF-03 and AQ-03 of the OpenCode audit).
//!
//! What kind of task is this run? The agent loop used to ask two different keyword lists: `code_verification::goal_is_coding` (13 words and 10 phrases; it decided the coding prompt,
//! the 24 rounds, read-before-write, the plan, the question, the verification of the answer) and `laya_validator::goal_is_coding_or_files` (verbs and project words; it decided the
//! route and the tool packs). "Implement pagination for the users endpoint in src/api.rs" was on the second list and not on the first: it was shown the file and terminal tools and
//! then run with the general PC-agent prompt, 12 rounds and none of the guards.
//!
//! Now a run has one [`RunProfile`] (`Coding`, `Files`, `Web` or `Desktop`), decided once when it starts by [`profile_for`] and read by every gate through [`coding`],
//! [`coding_latched`] and [`coding_or_files`]:
//!
//! * a caller may say what the task is (`"profile"` in the request of `POST /api/v1/mcp/agent`): what it says wins, and a run that goes on from a stopped one keeps the profile
//!   its record has;
//! * otherwise the keyword layer decides, as it did, plus a rule that reads a path and a word: a request to work on code or files that names a source file (`src/api.rs`) or a word of
//!   code (`endpoint`, `build`, `dependencies`, `tests`...) is `Coding`, the same request without either is `Files`;
//! * a run that began as something else and then changed a source file or ran a check is latched to coding for the rest of the run ([`latch_if_code_run`]): the record, the stuck
//!   policy and the guards that come after an edit apply to it. What was decided at the start (the prompt, the tools, the rounds) is not changed in the middle of a run.
//!
//! The profile only widens what *tightens* (the coding prompt, read before write, the verification of the answer, the list of tools the coding run is not shown). It never loosens a
//! permission: every call still goes through the tool host.
//!
//! The profile of a run is kept in a thread-local for the thread the run is on, like the context and the caller of a run are. A gate asks with the goal it is looking at: the goal of
//! the run, or the goal the person amended while the run went on (it starts with the goal of the run), gets the profile of the run; any other text gets the keyword answer, so that
//! a function called with some other text (an experience search, a test) is not told what the run is.

use crate::modules::code_verification::{goal_is_coding, path_is_code, run_is_coding};
use serde::Serialize;
use serde_json::Value;
use std::cell::{Cell, RefCell};

/// What kind of task a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunProfile {
    /// Work on code: read before write, the plan, one question, 24 rounds, the coding prompt, an answer that is checked against a test, a build or a lint run.
    Coding,
    /// Work on files that are not code (notes, settings, a folder): the file and terminal tools, the general prompt.
    Files,
    /// A look-up or a task on a web page.
    Web,
    /// A task on this computer (an app, a window): the general PC agent. Also what a goal that is none of the others is.
    Desktop,
}

impl RunProfile {
    /// The four, for the one place that lists them (the request field's error).
    pub const NAMES: &'static str = "coding, files, web or desktop";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::Files => "files",
            Self::Web => "web",
            Self::Desktop => "desktop",
        }
    }

    /// One of the four words, in any case. Anything else (a synonym, an empty word) is not a profile.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "coding" => Some(Self::Coding),
            "files" => Some(Self::Files),
            "web" => Some(Self::Web),
            "desktop" => Some(Self::Desktop),
            _ => None,
        }
    }

    /// Does the run work on code or files (the router shows it the file and terminal tools)?
    pub fn works_on_files(self) -> bool {
        matches!(self, Self::Coding | Self::Files)
    }
}

/// The `profile` field of a request: absent is no profile; a word that is not one of the four is an error, as a sentence for the 400.
pub fn parse_requested(field: Option<&str>) -> Result<Option<RunProfile>, String> {
    let Some(text) = field else { return Ok(None) };
    RunProfile::parse(text).map(Some).ok_or_else(|| {
        let shown: String = text.trim().chars().filter(|c| !c.is_control()).take(24).collect();
        format!("profile is {} (got {shown:?})", RunProfile::NAMES)
    })
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// "Write a function that reverses a string" asks for a snippet, which is answered in chat: these phrases alone do not make a request work on code (the router has said so since the
/// card that made it). With a file or a project named ("add a function to utils.py") they are work on it.
const SNIPPETS: [&str; 3] = ["write a function", "implement a function", "add a function"];

/// Words that say the work is on code. A request that is work on code or files (a doing verb and a file or a project word) and has one of these is `Coding`; without one it is `Files`
/// ("edit my notes", "update the config file"). `function` and `class` are here and not among the project words of the router: alone they ask for a snippet, with a project they are code.
const CODE_WORDS: &[&str] = &[
    "function", "method", "class", "struct", "trait", "endpoint", "api", "module", "crate", "repo", "repository", "codebase", "test", "tests", "build", "compile", "dependencies", "dependency",
    "deps", "lint", "bug", "bugs", "migration", "component", "unit", "refactor", "typecheck",
];

/// Words that say the task is on a web page or the web.
const WEB_WORDS: &[&str] = &["browser", "website", "webpage", "url", "google", "chrome", "edge", "firefox", "gmail", "whatsapp", "youtube", "internet", "online", "http", "https", "www"];

fn words_of(lower: &str) -> Vec<&str> {
    lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect()
}

/// Does the goal name a source file (`src/api.rs`, `utils.py`, `C:\work\app.tsx`)? A document or a setting (`notes.txt`, `settings.json`) is not one.
fn names_source_file(goal: &str) -> bool {
    goal.split_whitespace().any(|token| {
        let token = token.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '\\' | '-' | ':'))).trim_end_matches('.');
        path_is_code(token)
    })
}

/// The old keyword layer, without the phrases that only ask for a snippet.
fn keyword_coding(lower: &str) -> bool {
    let mut rest = lower.to_string();
    for s in SNIPPETS {
        rest = rest.replace(s, " ");
    }
    goal_is_coding(&rest)
}

/// What a goal is when nobody said, and nothing says it is work on code or files: a web look-up or a task on a page, or the general PC agent.
fn surface_profile(goal: &str, words: &[&str]) -> RunProfile {
    let on_the_web = crate::laya_validator::needs_web_lookup(goal)
        || crate::laya_validator::looks_live_info(goal)
        || words.iter().any(|w| WEB_WORDS.contains(w))
        || goal.split_whitespace().any(|t| {
            let t = t.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_lowercase();
            t.ends_with(".com") || t.ends_with(".org") || t.ends_with(".net")
        });
    if on_the_web {
        RunProfile::Web
    } else {
        RunProfile::Desktop
    }
}

/// The profile of a goal. `requested` (the caller's word) wins. Otherwise:
///
/// 1. `Coding` when the keyword layer says so (`refactor`, `cargo`, `failing test`, `fix the build`...), or when the goal is work on code or files and names a source file or a word of code;
/// 2. `Files` when it is work on code or files and names neither;
/// 3. `Web` for a look-up or a task on a page, else `Desktop`.
///
/// A question about code that names none of the old keywords ("what is an endpoint") is answered in chat and is none of the first two: the router's rule (a doing verb and a file or a
/// project) leaves questions out. One that names a keyword ("what is a failing test") is `Coding` as it always was: the keyword layer never left questions out.
pub fn profile_for(goal: &str, requested: Option<RunProfile>) -> RunProfile {
    if let Some(profile) = requested {
        return profile;
    }
    let lower = goal.to_ascii_lowercase();
    if keyword_coding(&lower) {
        return RunProfile::Coding;
    }
    let words = words_of(&lower);
    if crate::laya_validator::goal_is_coding_or_files(goal) {
        let code = names_source_file(goal) || words.iter().any(|w| CODE_WORDS.contains(w)) || crate::laya_validator::asks_to_make_pass(goal);
        return if code { RunProfile::Coding } else { RunProfile::Files };
    }
    surface_profile(goal, &words)
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The profile of the run on this thread
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// What a run on this thread was decided to be.
struct RunContext {
    /// The goal the profile was decided for.
    goal: String,
    profile: RunProfile,
    /// The caller said it: nothing the run does changes it.
    explicit: bool,
    /// The run changed a source file or ran a check ([`latch_if_code_run`]).
    latched: bool,
}

thread_local! {
    static REQUESTED: Cell<Option<RunProfile>> = const { Cell::new(None) };
    static RUN: RefCell<Option<RunContext>> = const { RefCell::new(None) };
}

/// Runs `f`, and every agent run it starts on this thread has `requested` as its profile (what the caller said, `None` for nothing). The previous setting comes back when `f` ends,
/// however it ends.
pub fn with_requested_profile<R>(requested: Option<RunProfile>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<RunProfile>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUESTED.with(|r| r.set(self.0));
        }
    }
    let _restore = Restore(REQUESTED.with(|r| r.replace(requested)));
    f()
}

/// What the caller said the runs of this thread are, if it said.
pub fn requested() -> Option<RunProfile> {
    REQUESTED.with(|r| r.get())
}

/// Decides the profile of the run for `goal` and holds it while `f` runs: the gates of the loop, the record of the run and the check of its answer all read this one. When the thread
/// already holds the profile of this goal (the run was started from another entry that decided it) nothing is decided again. The previous profile comes back when `f` ends.
pub fn scope<R>(goal: &str, f: impl FnOnce() -> R) -> R {
    let goal = goal.trim();
    if RUN.with(|r| r.borrow().as_ref().is_some_and(|c| c.goal == goal)) {
        return f();
    }
    struct Restore(Option<RunContext>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUN.with(|r| *r.borrow_mut() = self.0.take());
        }
    }
    let requested = requested();
    let context = RunContext { goal: goal.to_string(), profile: profile_for(goal, requested), explicit: requested.is_some(), latched: false };
    let _restore = Restore(RUN.with(|r| r.borrow_mut().replace(context)));
    f()
}

/// The run on this thread, when `goal` is its goal or what the person made of it while it went on: its profile, whether it was latched, and whether the caller said it.
fn held_for(goal: &str) -> Option<(RunProfile, bool, bool)> {
    RUN.with(|r| r.borrow().as_ref().filter(|c| !c.goal.is_empty() && goal.trim().starts_with(c.goal.as_str())).map(|c| (c.profile, c.latched, c.explicit)))
}

/// The profile of this goal: the run's when the thread holds the run of this goal, else what the keyword layer and the rule of the path and the word make of it.
pub fn profile_of(goal: &str) -> RunProfile {
    held_for(goal).map(|(p, _, _)| p).unwrap_or_else(|| profile_for(goal, None))
}

/// Is this a coding task? The gates that shape what the run is given (the prompt, the tools, the rounds, the plan, the question) ask this: it is decided once, when the run starts.
pub fn coding(goal: &str) -> bool {
    profile_of(goal) == RunProfile::Coding
}

/// [`coding`], or a run that turned into one: it changed a source file or ran a check ([`latch_if_code_run`]). The gates that act on what a run did (the record, the stuck policy,
/// the guard of an edit, the check of the answer, a reply that was cut off) ask this.
pub fn coding_latched(goal: &str) -> bool {
    match held_for(goal) {
        Some((profile, latched, _)) => profile == RunProfile::Coding || latched,
        None => profile_for(goal, None) == RunProfile::Coding,
    }
}

/// The router's question: is this a request to work on code or files, so that the file and terminal tools are shown and the gate sends it to the agent instead of answering in chat?
/// What the caller said about the run wins; otherwise it is `laya_validator::goal_is_coding_or_files`, which leaves a question out ("how do I fix the failing test in main.rs" is
/// answered in chat). It is not [`profile_of`]: a question that names a word of the old keyword layer is `Coding` for the guards of a run that was started anyway, and is still a
/// question for the gate.
pub fn routed_as_work(goal: &str) -> bool {
    match held_for(goal) {
        Some((profile, _, true)) => profile.works_on_files(),
        _ => crate::laya_validator::goal_is_coding_or_files(goal),
    }
}

/// Can the run on this thread still be latched ([`latch_if_code_run`])? It can when it began as something else and the caller did not give its profile. A cheap question, for the loop to
/// ask before it builds the list of what the run has done.
pub fn latch_pending() -> bool {
    RUN.with(|r| r.borrow().as_ref().is_some_and(|c| !c.explicit && !c.latched && c.profile != RunProfile::Coding))
}

/// The mode latch (task WF-03). A run that did not begin as a coding run and then changed a source file, or ran a test, a build or a lint command, is one: "make the page load
/// faster" turns out to be a change to `app.ts`. From then on [`coding_latched`] is true for it. A call that failed does not count, and neither does a run the caller gave a profile:
/// what it said stays. `calls` are the (tool, arguments, observation) of the run so far. Says whether the run was latched by this call.
pub fn latch_if_code_run(calls: &[(String, Value, String)]) -> bool {
    if !latch_pending() {
        return false;
    }
    let worked: Vec<(String, Value, String)> = calls.iter().filter(|(_, _, observation)| !crate::modules::code_verification::host_call_failed(observation)).cloned().collect();
    if !run_is_coding(&worked) {
        return false;
    }
    RUN.with(|r| {
        if let Some(c) = r.borrow_mut().as_mut() {
            c.latched = true;
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The goals of the contract (task AG-02): what each is, as a person would say it. Eight are coding and twelve are not; a change to this table is the owner's decision.
    const TABLE: &[(&str, RunProfile)] = &[
        // Coding
        ("implement pagination for the users endpoint in src/api.rs", RunProfile::Coding),
        ("rename foo to bar across the repo", RunProfile::Coding),
        ("fix the failing test in rust_probe", RunProfile::Coding),
        ("make the build pass", RunProfile::Coding),
        ("fix the failing build", RunProfile::Coding),
        ("upgrade the dependencies of the project", RunProfile::Coding),
        ("write tests for the parser", RunProfile::Coding),
        ("add an endpoint to the api folder", RunProfile::Coding),
        // Not coding
        ("open notepad", RunProfile::Desktop),
        ("what is the weather in Pune", RunProfile::Web),
        // A snippet for chat, not work on a project (the comment at `fallback.rs` has said so since the router was made).
        ("write a function that reverses a string", RunProfile::Desktop),
        ("edit my notes", RunProfile::Files),
        ("update the config file", RunProfile::Files),
        ("delete the old photos from my pictures folder", RunProfile::Files),
        ("write a poem about the sea", RunProfile::Desktop),
        ("fix me a drink", RunProfile::Desktop),
        ("build a sandcastle", RunProfile::Desktop),
        ("tell me today stock market condition", RunProfile::Web),
        ("open gmail and send an email to bob", RunProfile::Web),
        ("click the Save button in the window", RunProfile::Desktop),
    ];

    #[test]
    fn twenty_goals_get_the_profile_a_person_would_give_them() {
        assert_eq!(TABLE.len(), 20);
        assert_eq!(TABLE.iter().filter(|(_, p)| *p == RunProfile::Coding).count(), 8, "eight are coding and twelve are not");
        for (goal, want) in TABLE {
            assert_eq!(profile_for(goal, None), *want, "{goal}");
            assert_eq!(coding(goal), *want == RunProfile::Coding, "coding({goal})");
            assert_eq!(routed_as_work(goal), want.works_on_files(), "routed_as_work({goal})");
        }
    }

    /// The goals the widening is for (WF-03): each used to get the general prompt and 12 rounds, and now gets the coding ones; the questions and the chores stay what they were.
    #[test]
    fn the_goals_that_missed_the_coding_guards_get_them_and_the_chores_do_not() {
        for goal in [
            "add an endpoint to the api folder of the python project py_unittest that returns the total",
            "update dependencies",
            "could you please fix the type error in app.tsx",
            "write a function that reverses a string in utils.py",
            "write a function that reverses a string, then run cargo test",
            "make all the tests pass",
            "fix utils.py",
            "run the build and tell me what fails",
            "refactor the order service",
            "there is a type error in app.ts",
            "add a function to utils.py",
            "make it pass",
            "debug the migration in the repo",
        ] {
            assert_eq!(profile_for(goal, None), RunProfile::Coding, "{goal}");
        }
        for goal in [
            "explain what an endpoint is",
            "write a poem",
            "create a folder called holiday on my desktop",
            "edit README.md",
            "update settings.json",
            "build a house",
            "open the browser and search for flights",
        ] {
            // (A question that names a word of the old layer is the old layer's, as the next test says: "what is a failing test" is Coding there and was.)
            assert_ne!(profile_for(goal, None), RunProfile::Coding, "{goal}");
        }
        // A path alone is not work on code when the file is a document, and a verb alone is not work on files.
        assert_eq!(profile_for("edit README.md", None), RunProfile::Files);
        assert_eq!(profile_for("create a folder called holiday on my desktop", None), RunProfile::Files);
        assert_eq!(profile_for("open the browser and search for flights", None), RunProfile::Web);
    }

    /// What the keyword layer made coding before is coding still, the questions among them: nothing that had the guards loses them (the one exception is the table's snippet).
    #[test]
    fn what_the_keyword_layer_made_coding_before_is_coding_still() {
        for goal in [
            "fix the failing test in the parser",
            "refactor the order service",
            "make the tests pass",
            "cargo build fails, fix it",
            "fix the build",
            "there is a type error in app.ts",
            "how do I fix the failing test in main.rs",
            "run pytest and tell me what fails",
        ] {
            assert!(goal_is_coding(goal), "premise: {goal}");
            assert_eq!(profile_for(goal, None), RunProfile::Coding, "{goal}");
        }
    }

    #[test]
    fn what_the_caller_says_wins_in_both_directions() {
        assert_eq!(profile_for("hello", Some(RunProfile::Coding)), RunProfile::Coding);
        assert_eq!(profile_for("fix the failing test in rust_probe", Some(RunProfile::Web)), RunProfile::Web);
        assert_eq!(profile_for("open notepad", Some(RunProfile::Files)), RunProfile::Files);
        assert!(Some(RunProfile::Coding).is_some_and(|p| p.works_on_files()) && !RunProfile::Web.works_on_files() && !RunProfile::Desktop.works_on_files());
    }

    #[test]
    fn the_field_of_a_request_is_one_of_four_words_or_an_error_that_says_so() {
        assert_eq!(parse_requested(None), Ok(None));
        for (word, want) in [("coding", RunProfile::Coding), ("Files", RunProfile::Files), (" WEB ", RunProfile::Web), ("desktop", RunProfile::Desktop)] {
            assert_eq!(parse_requested(Some(word)), Ok(Some(want)), "{word}");
            assert_eq!(RunProfile::parse(want.as_str()), Some(want));
        }
        for bad in ["", "code", "coder", "all", "coding,web", "coding\nweb", "0"] {
            let e = parse_requested(Some(bad)).unwrap_err();
            assert!(e.starts_with("profile is coding, files, web or desktop"), "{bad:?}: {e}");
        }
        // The word that was refused is shown, cut, and with no control character in it.
        let long = format!("{}\u{7}x", "a".repeat(100));
        let e = parse_requested(Some(&long)).unwrap_err();
        assert!(e.len() < 100 && !e.contains('\u{7}'), "{e}");
    }

    #[test]
    fn a_run_holds_its_profile_and_the_gates_read_it_for_its_goal_and_for_what_was_added_to_it() {
        // Outside a run the keyword answer is given.
        assert!(!coding("add some text"));
        with_requested_profile(Some(RunProfile::Coding), || {
            assert_eq!(requested(), Some(RunProfile::Coding));
            scope("add some text", || {
                assert!(coding("add some text") && coding_latched("add some text") && routed_as_work("add some text"));
                // The goal as the person amended it while the run went on starts with the goal of the run.
                assert!(coding("add some text\nThe person added while you were working: and more"));
                // Any other text is not the run's: a function called with some other text is not told what the run is.
                assert!(!coding("open notepad") && !coding("something else"));
                // A goal that is empty is nobody's.
                assert!(!coding(""));
            });
            assert!(!coding("add some text"), "the profile of a run ends with it");
        });
        assert_eq!(requested(), None, "and so does what the caller said");
        // A run for which nothing was said decides by the words.
        scope("rename foo to bar across the repo", || {
            assert!(coding("rename foo to bar across the repo"));
            assert_eq!(profile_of("rename foo to bar across the repo"), RunProfile::Coding);
        });
        scope("open notepad", || assert_eq!(profile_of("open notepad"), RunProfile::Desktop));
    }

    #[test]
    fn the_router_asks_its_own_question_and_what_the_caller_said_wins_over_it() {
        // A question about code is not a task for the gate (it is answered in chat), though a run that was started anyway has the guards of a coding run.
        let question = "how do I fix the failing test in main.rs";
        assert!(!routed_as_work(question) && coding(question));
        assert!(routed_as_work("fix the failing test in main.rs") && routed_as_work("make the build pass") && routed_as_work("rename foo to bar across the repo"));
        assert!(!routed_as_work("write a function that reverses a string") && !routed_as_work("open notepad"));
        // What the caller said is the answer, both ways.
        with_requested_profile(Some(RunProfile::Files), || scope(question, || assert!(routed_as_work(question))));
        with_requested_profile(Some(RunProfile::Coding), || scope("hello", || assert!(routed_as_work("hello"))));
        with_requested_profile(Some(RunProfile::Web), || scope("fix the failing test in rust_probe", || assert!(!routed_as_work("fix the failing test in rust_probe"))));
        // A profile the words made is not what the caller said: the router's rule stays the router's.
        scope(question, || assert!(!routed_as_work(question)));
    }

    #[test]
    fn a_profile_that_was_said_is_not_changed_by_a_second_start_for_the_same_goal_but_a_new_goal_gets_its_own() {
        with_requested_profile(Some(RunProfile::Web), || {
            scope("fix the failing test in rust_probe", || {
                assert_eq!(profile_of("fix the failing test in rust_probe"), RunProfile::Web);
                // The same goal again (an entry that was started from another entry): nothing is decided again.
                scope("fix the failing test in rust_probe", || assert_eq!(profile_of("fix the failing test in rust_probe"), RunProfile::Web));
                // A run of another goal inside it is its own, and the first comes back when it ends.
                with_requested_profile(None, || {
                    scope("open notepad", || assert_eq!(profile_of("open notepad"), RunProfile::Desktop));
                });
                assert_eq!(profile_of("fix the failing test in rust_probe"), RunProfile::Web);
            });
        });
    }

    fn call(tool: &str, args: Value, observation: &str) -> (String, Value, String) {
        (tool.into(), args, observation.into())
    }

    #[test]
    fn a_run_that_edits_code_or_runs_a_check_is_a_coding_run_from_then_on_unless_the_caller_said_otherwise() {
        let goal = "make the page load faster";
        scope(goal, || {
            assert!(!coding(goal) && !coding_latched(goal));
            // Reading changes nothing; a failed edit did not happen; a note is not code.
            assert!(!latch_if_code_run(&[call("fs.read", json!({"path": "app.ts"}), "text")]));
            assert!(!latch_if_code_run(&[call("fs.edit", json!({"path": "src/app.ts"}), "ERROR [NOT_FOUND] fs.edit: no such file")]));
            assert!(!latch_if_code_run(&[call("fs.edit", json!({"path": "notes.txt"}), "Edited notes.txt")]));
            assert!(!coding_latched(goal));
            // A source file changed: the run is a coding run, and the profile it was decided to be is still the one it started with.
            assert!(latch_if_code_run(&[call("fs.edit", json!({"path": "src/app.ts"}), "Edited src/app.ts")]));
            assert!(coding_latched(goal), "the guards that come after an edit apply");
            assert!(!coding(goal), "what was decided at the start (the prompt, the tools, the rounds) is not changed in the middle of a run");
            assert_eq!(profile_of(goal), RunProfile::Desktop);
            assert!(!latch_if_code_run(&[call("fs.edit", json!({"path": "src/app.ts"}), "Edited src/app.ts")]), "it is latched once");
        });
        assert!(!coding_latched(goal), "and not for the next run");
        // A check is a sign too.
        scope("tidy up", || {
            assert!(latch_if_code_run(&[call("term.run", json!({"exe": "cargo", "args": ["test"]}), "exit code 0")]));
        });
        // What the caller said stays.
        with_requested_profile(Some(RunProfile::Desktop), || {
            scope(goal, || {
                assert!(!latch_if_code_run(&[call("fs.edit", json!({"path": "src/app.ts"}), "Edited src/app.ts")]));
                assert!(!coding_latched(goal));
            });
        });
        // Outside a run there is nothing to latch.
        assert!(!latch_if_code_run(&[call("fs.edit", json!({"path": "src/app.ts"}), "Edited src/app.ts")]));
    }
}
