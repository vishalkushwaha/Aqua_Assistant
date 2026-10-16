//! The typed flag registry (card OA3-03; task EX-19 of the OpenCode audit).
//!
//! About 160 `AQUA_*` names are read from the environment where they are used, each with its own spelling of "on" (`on|1|true`, `on|1|true|yes`, "anything but `0|false|off`" ...). A test
//! that wanted a flag set it in the process environment, which every other test thread shares (the comment that used to be at the end of `tool_search.rs` said so). This module
//! is the one place the ten flags below are read:
//!
//! * a flag is a [`Flag`] in the table [`ALL`]: its name, its kind (switch that is off unless it says on, switch that is on unless it says off, one of a few words, a number), its
//!   default, and what it does. [`is_on`], [`choice`] and [`int`] read it. **The reading is exactly what each caller did before**, so no default changes: the parity test in this file
//!   runs the old readers against the new ones over a matrix of values.
//! * the value is read from the environment each time it is asked for, as before: the core sets the signed promoted flags (`promoted.rs`) into the environment at start, and a person's own
//!   variable wins over them there; this module adds nothing to that and takes nothing from it. A flag that widens what the agent may reach is marked [`Flag::reach`], and a test
//!   keeps every such flag out of what a promotion may set (`promoted::ALLOWED`).
//! * a test does not touch the process environment: [`with_overrides`] sets values for the current thread while a closure runs, and [`Flags::for_test`] makes a [`Flags`] that never looks at
//!   the environment at all.
//! * `GET /api/v1/flags` lists the names, defaults and current values (UI token), and the table in `CONFIGURATION.md` is generated from [`ALL`] ([`markdown_table`]).
//!
//! Only these flags are in the registry so far; the other names are still found by the source scan that generates the second table of `CONFIGURATION.md`.

use std::cell::RefCell;
use std::collections::BTreeMap;

/// How a flag is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Off unless the value is one of `on` (after trimming, and lower-casing when `fold_case`).
    OptIn { on: &'static [&'static str], fold_case: bool },
    /// On unless the value is one of `off` (after trimming, and lower-casing when `fold_case`).
    OptOut { off: &'static [&'static str], fold_case: bool },
    /// One of a few words: each choice has a name and the spellings that mean it; anything else is the default.
    Choice { choices: &'static [(&'static str, &'static [&'static str])] },
    /// A whole number within bounds; anything else is the default.
    Int { min: i64, max: i64 },
}

/// One flag.
#[derive(Clone, Copy, Debug)]
pub struct Flag {
    /// The environment variable.
    pub name: &'static str,
    pub kind: Kind,
    /// What it is when the variable is not set (`off`, `on`, a choice, a number); empty for a variable that has no default (an older name another flag reads too).
    pub default: &'static str,
    /// One line for the person.
    pub doc: &'static str,
    /// Switching it on gives the agent something it could not reach before (a tool, the clipboard, the screen). Such a flag is the person's: a promotion may not set it.
    pub reach: bool,
}

impl Flag {
    const fn opt_in(name: &'static str, on: &'static [&'static str], doc: &'static str) -> Flag {
        Flag { name, kind: Kind::OptIn { on, fold_case: true }, default: "off", doc, reach: false }
    }

    const fn opt_out(name: &'static str, off: &'static [&'static str], doc: &'static str) -> Flag {
        Flag { name, kind: Kind::OptOut { off, fold_case: true }, default: "on", doc, reach: false }
    }

    /// A whole number from `min` to `max` (a time in seconds, a count); `default` is what it is when the variable is not set.
    const fn int(name: &'static str, min: i64, max: i64, default: &'static str, doc: &'static str) -> Flag {
        Flag { name, kind: Kind::Int { min, max }, default, doc, reach: false }
    }

    const fn reach(mut self) -> Flag {
        self.reach = true;
        self
    }

    /// Compare the value as typed, without lower-casing it (`AQUA_ROUTER_V2=OFF` has never turned the router back).
    const fn exact(mut self) -> Flag {
        self.kind = match self.kind {
            Kind::OptIn { on, .. } => Kind::OptIn { on, fold_case: false },
            Kind::OptOut { off, .. } => Kind::OptOut { off, fold_case: false },
            other => other,
        };
        self
    }

    /// The word for the kind, in the status and the docs.
    pub fn kind_word(&self) -> &'static str {
        match self.kind {
            Kind::OptIn { .. } | Kind::OptOut { .. } => "switch",
            Kind::Choice { .. } => "choice",
            Kind::Int { .. } => "number",
        }
    }

    /// The spellings that change it from its default, for the docs.
    pub fn spellings(&self) -> String {
        match self.kind {
            Kind::OptIn { on, fold_case } => format!("{} turn it on{}", on.join(", "), if fold_case { "" } else { " (exactly as written)" }),
            Kind::OptOut { off, fold_case } => format!("{} turn it off{}", off.join(", "), if fold_case { "" } else { " (exactly as written)" }),
            Kind::Choice { choices } => choices.iter().map(|(name, words)| if words.is_empty() { (*name).to_string() } else { format!("{name} ({})", words.join(", ")) }).collect::<Vec<_>>().join("; "),
            Kind::Int { min, max } => format!("a whole number from {min} to {max}"),
        }
    }

    /// The value for `raw` (the text of the variable, or `None` when it is not set), without the environment: what each kind makes of it.
    pub fn value_of(&self, raw: Option<&str>) -> String {
        match self.kind {
            Kind::OptIn { .. } | Kind::OptOut { .. } => (if self.parse_switch(raw) { "on" } else { "off" }).to_string(),
            Kind::Choice { .. } => self.parse_choice(raw).to_string(),
            Kind::Int { .. } => self.parse_int(raw).to_string(),
        }
    }

    /// A switch: on or off.
    pub fn parse_switch(&self, raw: Option<&str>) -> bool {
        let word = |fold: bool, v: &str| if fold { v.trim().to_ascii_lowercase() } else { v.trim().to_string() };
        match self.kind {
            Kind::OptIn { on, fold_case } => raw.map(|v| word(fold_case, v)).is_some_and(|v| on.contains(&v.as_str())),
            Kind::OptOut { off, fold_case } => !raw.map(|v| word(fold_case, v)).is_some_and(|v| off.contains(&v.as_str())),
            _ => false,
        }
    }

    /// A choice: its name, or the default when the value is not one of the spellings.
    pub fn parse_choice(&self, raw: Option<&str>) -> &'static str {
        match self.kind {
            Kind::Choice { choices } => {
                let v = raw.map(|v| v.trim().to_ascii_lowercase());
                choices.iter().find(|(name, words)| v.as_deref().is_some_and(|v| v == *name || words.contains(&v))).map(|(name, _)| *name).unwrap_or(self.default)
            }
            _ => self.default,
        }
    }

    /// A number: the value when it is a whole number within bounds, else the default.
    pub fn parse_int(&self, raw: Option<&str>) -> i64 {
        let default = self.default.parse().unwrap_or(0);
        match self.kind {
            Kind::Int { min, max } => raw.and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| (min..=max).contains(n)).unwrap_or(default),
            _ => default,
        }
    }
}

const ON_1_TRUE: &[&str] = &["on", "1", "true"];
const ON_1_TRUE_YES: &[&str] = &["on", "1", "true", "yes"];

/// Defines the flags and the table that lists them, so that a flag cannot be defined and left out of the table.
macro_rules! flags {
    ($( $(#[$m:meta])* $id:ident = $flag:expr; )*) => {
        $( $(#[$m])* pub const $id: Flag = $flag; )*
        /// Every flag in the registry, in the order the docs list them.
        pub static ALL: &[&Flag] = &[ $( &$id ),* ];
    };
}

flags! {
    /// Shows the agent `tools.search`, which finds a tool by what you want to do. Off until the benchmark shows it helps.
    TOOL_SEARCH = Flag::opt_in("AQUA_TOOL_SEARCH", ON_1_TRUE, "Shows the agent the tools.search tool, which finds a tool by what you want to do.").reach();
    /// Browser and desktop calls through the tool host: the comparison runs by default (`shadow`), `on` makes the host decide, `off` stops the comparison.
    DOOR = Flag {
        name: "AQUA_DOOR",
        kind: Kind::Choice { choices: &[("on", ON_1_TRUE), ("off", &["off", "0", "false"])] },
        default: "shadow",
        doc: "Whether browser and desktop calls go through the tool host: shadow compares the two routes and changes nothing, on makes the host decide, off stops the comparison. AQUA_ONE_DOOR=off also turns it off.",
        reach: false,
    };
    /// The older name of `AQUA_DOOR`; only `off` is read.
    ONE_DOOR = Flag {
        name: "AQUA_ONE_DOOR",
        kind: Kind::Choice { choices: &[("off", &["off", "0", "false"]), ("on", ON_1_TRUE)] },
        default: "",
        doc: "The older name of AQUA_DOOR. Only off is read (it turns the comparison off).",
        reach: false,
    };
    /// The second generation of the router; `AQUA_ROUTER_V2=0` brings back the earlier rules and tool order.
    ROUTER_V2 = Flag::opt_out("AQUA_ROUTER_V2", &["0", "false", "off"], "The second generation of the router (which tools a goal is shown). Set 0, false or off to bring back the earlier rules.").exact();
    /// Deep research (`research.run`): the tool exists only when this is on.
    RESEARCH = Flag::opt_in("AQUA_RESEARCH", ON_1_TRUE_YES, "Deep research: the research.run tool exists only when this is on.").reach();
    /// Drafts a skill by itself after a run that went well; the person reads every draft before it is a skill.
    SKILL_PROPOSALS = Flag::opt_in("AQUA_SKILL_PROPOSALS", ON_1_TRUE_YES, "Drafts a skill by itself after a run that went well. Nothing becomes a skill until you have read and approved it.");
    /// The clipboard tools; the agent has never been able to reach the clipboard, and a person who wants it says so.
    CLIPBOARD_TOOLS = Flag::opt_in("AQUA_CLIPBOARD_TOOLS", ON_1_TRUE_YES, "Offers the clipboard.read and clipboard.write tools. clipboard.read always asks.").reach();
    /// The tools that read the text on the screen; on unless this says no.
    SCREEN_TOOLS = Flag::opt_out("AQUA_SCREEN_TOOLS", &["0", "off", "false", "no"], "Offers the tools that read the text on the screen. Set 0, off, false or no to leave them out.").reach();
    /// Recipes version 2: placeholders and postconditions in a learned recipe and its replay.
    RECIPES_V2 = Flag::opt_in("AQUA_RECIPES_V2", ON_1_TRUE, "Recipes version 2: how a learned recipe is made, kept and replayed, with placeholders and postconditions.");
    /// A short plan before a multi-step goal.
    PLAN = Flag::opt_in("AQUA_PLAN", ON_1_TRUE, "The agent writes a short plan before a goal with two steps or more.");
    /// The reader that pulls facts, with quotes, from a page.
    READER = Flag::opt_in("AQUA_READER", ON_1_TRUE, "A reader that takes facts, each with its quote, from a page before the agent answers.");
    /// Keeps the whole text of a tool result that is cut for the model (card OA5-02). On unless this says no: a cut used to be for good.
    TOOL_OUTPUT_SPILL = Flag::opt_out("AQUA_TOOL_OUTPUT_SPILL", &["0", "off", "false", "no"], "Keeps the whole text of a tool result that is cut for the model, with its secrets masked, in the folder tool-output next to the data folder for a week, so the agent can search it with fs.grep. Set 0, off, false or no to keep nothing.");
    /// Sends the engine's calls to the language model server through the model router (card OA9-01). Off until the speed gate and the bake-off have been run with it on.
    MODEL_ROUTER = Flag::opt_in("AQUA_MODEL_ROUTER", ON_1_TRUE, "Sends the calls to the local language model through the model router (core/src/model) instead of straight to the server. The request and the answer are the same either way; this is for trying the router before it becomes the default.");
    /// Offers a coding run the two tools its loop already handles, `todo.write` and `agent.explore` (card OA5-01). Off: the prompt names neither, because the reply could not name them.
    AGENT_TOOLS = Flag::opt_in("AQUA_AGENT_TOOLS", ON_1_TRUE, "Offers a coding run the two tools its loop handles itself: todo.write (a checklist that is shown again every round) and agent.explore (one read-only search through the host, at most eight a run). Off until a benchmark on the model shows they help; with it off the coding prompt does not mention them.");
    /// One more pack of tools for a coding goal that names it (card OA5-01): up to two tools of the database, docker, kubernetes, monitoring, forge or web pack.
    CODING_PACKS = Flag::opt_in("AQUA_CODING_PACKS", ON_1_TRUE, "Shows a coding goal up to two tools of one more pack when its words call for one (database, docker, kubernetes, monitoring, forge or web), so at most 14 tools instead of 12. Off until a benchmark on the model shows the longer list does not cost accuracy.").reach();
    /// Keeps the start of a coding run's prompt the same from round to round and from goal to goal (card OA4-02). Off until the cache reuse and the accuracy of coding goals have been measured with it on.
    PREFIX_STABLE = Flag::opt_in("AQUA_PREFIX_STABLE", ON_1_TRUE, "Keeps the start of a coding goal's prompt the same from round to round and from goal to goal, so the model server reads less of it again: the coding record (what was read, changed and failed) moves to the end of the prompt, the rules use <file> and <folder> instead of the paths of the goal, and the goal's file, folder and check follow the list of tools. Off until the saving and the effect on coding goals have been measured.");
    /// Reads a repository's `.aqua` folder (skills, commands, agents, hooks, MCP servers, formatters, tools) for a run that works in it, each kind only after the person has trusted it by its hash (card OA11-01).
    PROJECT_PACK = Flag::opt_in("AQUA_PROJECT_PACK", ON_1_TRUE, "Looks for a .aqua folder in the project a goal names (skills, commands, agents, hooks, MCP servers, formatters, tools) and uses each kind of it only after you have trusted that kind by the hash of its files; a file that changes after that is untrusted again. Nothing in the folder is read, listed or run before you trust it.").reach();
    /// `skill.file`: reads the other files of a skill's folder (card OA11-01).
    SKILLS_V2 = Flag::opt_in("AQUA_SKILLS_V2", ON_1_TRUE, "Offers skill.file, which lists and reads the other files in a skill's folder (a template, a checklist): files directly in the folder, text, 16 KiB at most, never a link or a path.").reach();
    /// The last round a run is allowed may only be an answer (card OA5-04, TL-10). Off until the model has been run with it.
    LAST_STEP_FINAL = Flag::opt_in("AQUA_LAST_STEP_FINAL", ON_1_TRUE, "On the last round the run's budget allows (a budget of two rounds or more), the reply is held to a final answer (a question to you stays possible where asking is on) and the prompt says to answer from what the run has and to say what is not done, so a run that used all its rounds ends with the model's own words and not a template; it is still recorded as having stopped at max_steps. Off until it has been tried on the model.");
    /// `fs.outline`: the declarations of a source file with the lines each spans (card OA5-03, CX-17). Off until the number of rounds of the coding tasks has been measured with it on.
    FS_OUTLINE = Flag::opt_in("AQUA_FS_OUTLINE", ON_1_TRUE, "Offers fs.outline, which lists the declarations of a source file (functions, structs, classes) with the lines each one spans, so that a long file can be read by the part that is needed. Read through the same folder checks as fs.read. Off until the rounds a coding task needs have been measured with it on; with it off the tool does not exist and the coding prompt does not mention it.");
    /// Shows a result that a later one repeats byte for byte as one line that names the later one (card OA5-03, CX-09). Off until the rounds a coding task needs have been measured with it on.
    COLLAPSE_REPEATS = Flag::opt_in("AQUA_COLLAPSE_REPEATS", ON_1_TRUE, "Shows a tool result that a later one repeats, word for word, as one line that says which later result it is (the same file read again, the same check run again), so the prompt keeps the room for results that say something new. An error is never collapsed. Off until the rounds a coding task needs have been measured with it on.");
    /// Speaks to each model as its family and its profile say, and not in one fixed recipe (card OA9-03). Off until the speed gate and the bake-off have been run with it on.
    MODEL_PROFILES = Flag::opt_in("AQUA_MODEL_PROFILES", ON_1_TRUE, "Speaks to each language model as its family and its profile say (core/src/model/adapter.rs, data/models/profiles.json, data/models/router.json): a Qwen3 file gets /no_think and the thinking switch exactly as before, any other family gets neither, the thinking blocks are taken out of its reply by the rules of its family, and the sampling of its profile and of the role is sent. Off: every model is spoken to as Qwen3 is, with temperature 0.3 and top_p 0.9.");
    /// The sampling a model's card recommends for the roles that speak to the person (card OA9-03). Off: changing how chat sounds is an experiment that needs the bake-off.
    SAMPLING_PRESETS = Flag::opt_in("AQUA_SAMPLING_PRESETS", ON_1_TRUE, "Adds the sampling the model's card recommends to the chat and summary roles of a Qwen3 model (temperature 0.7, top_p 0.8, top_k 20, from the published card and not measured here), under what the profile and the role set. Off until the bake-off has been run with it.");
    /// A seed for the model's choice of words, so that a run can be repeated (card OA9-03). Not set: the model chooses by chance, as it always has.
    SEED = Flag { name: "AQUA_SEED", kind: Kind::Int { min: -1, max: i64::MAX }, default: "-1", doc: "A whole number from 0 up that is sent as the seed of every request to the local model, so that an evaluation can be run again with the same words. -1 or not set: no seed, and the model chooses by chance as it always has.", reach: false };
    /// How long a streamed answer of the local model may go quiet, once it has begun, before the call is hung up on (card OA9-02, task PR-03). 0 (off) until it has been tried with the model.
    STREAM_IDLE_SECS = Flag::int("AQUA_STREAM_IDLE_SECS", 0, 3600, "0", "How many seconds a streamed answer of the local model may go quiet, once its first piece has come, before the call is hung up on and ends with the message that the model went quiet (the model server is asked whether it is well, and keeps its model when it is). 0 is off, and is what it is until it has been tried with the model; 20 is the value the audit suggests. It does not apply to a call that is not streamed, which gets nothing until the whole answer is ready, and it does not run while the model is still reading the prompt.");
    /// How long a streamed call may wait for the first piece of the answer (card OA9-02). 0 (off) until it has been tried with the model.
    STREAM_FIRST_EVENT_SECS = Flag::int("AQUA_STREAM_FIRST_EVENT_SECS", 0, 3600, "0", "How many seconds a streamed call may wait for the model to begin to answer (it is reading the prompt first: about 8 seconds for a cold one, longer for a long one on the processor) before the call is hung up on with the same message. 0 is off, and is what it is until it has been tried with the model. The call's own time limit (AQUA_LLAMA_TIMEOUT_SECS) always applies.");
    /// `project.profile` and `check.run`: one structured way to test, build, lint and type-check a project (card OA4-05). Off until a benchmark on the model shows they help.
    CHECK_TOOLS = Flag::opt_in("AQUA_CHECK_TOOLS", ON_1_TRUE, "Offers project.profile (what the project is and the command that tests, builds, lints and type-checks it) and check.run (runs one of those commands through the terminal tools and answers with a short list of what failed and where) to a coding run, in place of the model choosing a command and a folder by itself. check.run asks as the same term.run would. Off until the rounds a coding task needs have been measured with it on; with it off neither tool is on offer (they are listed as unavailable) and the coding prompt does not mention them.").reach();
    /// Notes of the folders a file is read from, and the digest of a long note (card OA4-05, task CU-11). Off until the coding tasks have been measured with it on.
    NESTED_NOTES = Flag::opt_in("AQUA_NESTED_NOTES", ON_1_TRUE, "Attaches the note (AQUA.md, AGENTS.md or CLAUDE.md) of the folder a coding run reads a file from, once a run, to the result of the read, when you trusted that folder and the note is as it was when you did, and cuts a long project note at its headings and lines (the build, test and rule sections first) instead of at 2,048 characters wherever they fall. Off until it has been measured with the coding tasks.");
    /// A "no" that comes with words lets a coding run go on with them (card OA10-03, AG-11). Off: the words are not kept and a "no" stops the run, as it always did.
    DENY_FEEDBACK = Flag::opt_in("AQUA_DENY_FEEDBACK", ON_1_TRUE, "A \"no\" to a question can come with words (POST /api/v1/browser/confirm with decision deny and a reason, up to 300 characters). With this on, a coding run is shown the words as data from you (\"They said: ...\") and goes on instead of stopping, at most twice a run; a run that is not a coding run, a third \"no\" and a \"no\" with no words stop the run as they always did. The words never approve anything. Off until the app has a place to type them: then the words are not kept and every \"no\" stops the run.");
    /// "also ..." and "then ..." said while a run is going are added to it, or kept for after it (card OA10-03, UX-11). Off: they are new requests, as they always were.
    STEER_CUES = Flag::opt_in("AQUA_STEER_CUES", ON_1_TRUE, "A typed request that starts like a continuation while a task you are waiting for is going on is added to that task (\"also add tests\", \"wait, use b.rs instead\") or kept to run after it (\"then run clippy\", \"when you're done, ...\"), instead of waiting its turn as a new request. Never for a stop, never for a yes or a no (nothing said this way answers a question), and only with the UI token. Off: such a sentence is a new request, as it always was.");
    /// Aqua says, and the app shows, when a run needs you or has ended (card OA10-03, UX-13). Off: nothing is said unasked, as it never was for a run.
    ATTENTION = Flag::opt_in("AQUA_ATTENTION", ON_1_TRUE, "Puts a short sentence in the queue of spoken announcements when a run is waiting for your yes (\"I need your yes to edit main.rs\"), has a question for you, or has ended (\"Done: 3 files changed\", \"I stopped: I reached the limit of 200 tool calls\"). The app's player says it when it asks for the next announcement and it is not busy; nothing is said by this flag alone if no player is running. Built from fixed sentences and the name of the file, never code or a path; never a yes by voice. Off: nothing is announced for a run.");
    /// Custom commands: a procedure written once (`data/commands/<name>.md`, or a trusted repository's `.aqua/commands`) and started by name with a few words (card OA11-02).
    COMMANDS = Flag::opt_in("AQUA_COMMANDS", ON_1_TRUE, "Custom commands: a procedure you wrote once in data/commands/<name>.md (or in the commands of a repository whose .aqua you trusted) is listed by GET /api/v1/commands and started by name with a few words by POST /api/v1/commands/<name>/run. A line !`like this` in it is a step that is run through the tool host and asks like any command; the run starts with only the tools the command allows. Nothing is expanded or run until you start a command.");
    /// Declared command tools: `cmd.*` from `data/tools/custom.json` and `proj.*` from a trusted repository's `.aqua/tools.json` (card OA11-02).
    COMMAND_TOOLS = Flag::opt_in("AQUA_COMMAND_TOOLS", ON_1_TRUE, "Offers the command tools you declared: cmd.* from data/tools/custom.json and proj.* from the tools.json of a repository whose .aqua code you trusted. Each is a fixed program with typed blanks, run as the term.run call it stands for (the same rules, folder checks and question), never through a shell.").reach();
    /// The answer of a coding run ends with one sentence the program writes from the checks it saw (card OA4-04, WF-07). Off: the answer is the model's words only, as it always was.
    ANSWER_EVIDENCE = Flag::opt_in("AQUA_ANSWER_EVIDENCE", ON_1_TRUE, "Ends the answer of a coding run with one sentence that Aqua writes from the checks it saw (cargo test passed, the last check failed, the code changed after it, no check was run), so that what the answer claims and what the run showed are side by side. Off: the answer is the model's words only, as it always was.");
    /// The coding record built from what the checks said, and kept with the run (card OA4-04, WF-06). Off: the transcripts of the scenarios pin the record as it was, and a golden is the owner's to write again.
    CODING_RECORD = Flag::opt_in("AQUA_CODING_RECORD", ON_1_TRUE, "Builds the coding record that stands in front of the history of a coding run (what the goal is, what was read and changed, what the newest check said and which tests failed, whether the code changed since, what is in the way, what to do next) from what the checks said, so that it says to give the answer after a passing check and not to read the last error, and keeps it with the run (record of GET /api/v1/runs/{id}). Off: the record keeps the first error of the run for ever, as it always did. Turning it on changes the text of every coding prompt: write the golden transcripts of core/tests/fixtures/agent_scenarios again (python scripts/eval/record_scenario.py golden) when it becomes the default.");
    /// The check the loop runs after an edit is a step of the run (card OA4-04, ED-15). Off: it is printed under the edit's answer and is no step, as the golden transcripts pin.
    CHECK_STEPS = Flag::opt_in("AQUA_CHECK_STEPS", ON_1_TRUE, "Records the check the loop runs after an edit (python -m py_compile after a Python edit, cargo check or npm run typecheck with AQUA_CODING_CHECK=on) as a step of the run, a term.run marked automatic, so that the gate that asks for a passing check after the last change reads it and an answer that follows a passing check is not sent back to run one more. Off: the check is printed under the answer of the edit and is no step, as it always was. Turning it on changes the steps of a run: write the golden transcripts of core/tests/fixtures/agent_scenarios again (python scripts/eval/record_scenario.py golden) when it becomes the default.");
    /// The output of the check that follows an edit, reduced to the exit code and at most eight of the compiler's complaints (card OA4-04, ED-15). Off until the rounds a coding task needs have been measured with it.
    SHORT_CHECK_OUTPUT = Flag::opt_in("AQUA_SHORT_CHECK_OUTPUT", ON_1_TRUE, "Shows the model the output of the check that follows an edit (cargo check, npm run typecheck, python -m py_compile) as its exit code, at most eight of the compiler's complaints with their file and line, and how many more there were, instead of the whole text (the last lines of the traceback for a Python file that does not compile). The gate that asks for a passing check still reads the whole of it. Off: the model sees the whole text, as it always did.");
    /// Checkpoints of a trusted project around a run (card OA7-03, ED-11 and CX-11). Off: nothing is kept, and a command's effects are not in the run diff.
    SNAPSHOT = Flag::opt_in("AQUA_SNAPSHOT", ON_1_TRUE, "Takes a checkpoint of the project folder of a coding run, when it is a trusted one (the workspace, or the project folder under full access), before the run's first command that may change files and again when the run ends, in a store of Aqua's own under data/snapshots (at most 512 MiB; a file over 2 MB, an ignored file, a secret-looking name and Aqua's own files are never kept). GET /api/v1/runs/{id}/diff then shows what the run changed, whatever changed it (the files each of the run's tools changed and the files a command changed), and Undo this run puts back what a command changed besides what the file tools changed, leaving every file you changed since. Off until it has been measured on the disk and in time: nothing is kept and a command's changes are not in the run diff.");
    /// Coding sessions: what a coding run came to is kept per project for a few hours and shown to the next run of the project as a short block (card OA10-04, AG-16 and CX-08). Off until the rounds a second coding goal needs have been measured with it on.
    CODING_SESSIONS = Flag::opt_in("AQUA_CODING_SESSIONS", ON_1_TRUE, "Keeps a short record of what a coding run did (the goals, the names of the files it read and changed, what its last check said, its plan, a few notes of its own) per project, and shows it to the next coding run in the same project as one block of at most 600 characters of data, not instructions: 4 hours after the last run for a session found by its project, 24 hours for one you named with session_id. No file contents are kept. The run gets agent.note to write at most 12 notes of 160 characters. The records are deleted for good (the file is overwritten) after 30 days, when you delete one, and when you clear the conversation. Off: nothing is kept and every prompt is the same as before. Turning it on lengthens the system part of a coding prompt by up to 600 characters: run scripts/eval/speed_gate.py with Aqua closed before it becomes a default.");
    /// A coding run that stops on its rounds or its time says where it stopped, and one that is getting somewhere is given 8 more rounds once (card OA10-04, WF-12). Off: the run ends at its cap, as it always did.
    RUN_CONTINUE = Flag::opt_in("AQUA_CONTINUE", ON_1_TRUE, "A coding run that is about to stop at its last round while the failing tests of its newest check have become fewer in the last three rounds is given 8 more rounds, once, and says so; a coding run that stops on its rounds or its time ends its answer with a Where I stopped line that POST /api/v1/runs/{id}/continue can go on from. A chain of runs is at most 48 rounds and 30 minutes. Off: a run ends at its round cap and the answer is the one it always was. Turning it on changes how long a run may take: measure the rounds of the coding tasks before it becomes a default.");
    /// A summary of a closed coding session is stored as experience when Aqua is idle (card OA10-04, CX-19). Experimental.
    SESSION_SUMMARY = Flag::opt_in("AQUA_SESSION_SUMMARY", ON_1_TRUE, "Experimental. When a coding session has been closed and Aqua has been idle for ten minutes, writes one short summary of it (the goal, the files changed, what the last check said, what is left) from the session record alone, with no model call and no file contents, and stores it as a project note that a later goal in the same project may be offered as an untrusted note. At most one summary per session. Off: no summary is written.");
    /// The person can pre-approve a project's checks for one run (card OA10-05, WF-14, decision D-07). Off: no pre-approval can be made, given to a run or used.
    PREAPPROVALS = Flag::opt_in("AQUA_PREAPPROVALS", ON_1_TRUE, "Lets you, in the app (the UI token) or at a terminal (aqua-cli preapprove), allow ONE run to run the project's own checks (the tests, the build, the linter, the type check: only the command lines the terminal tools count as a project's checks) in ONE folder without asking each time, for a number of calls and a time you choose, and lets a run be started with that pre-approval (POST /api/v1/runs with preapproval_id, aqua-cli run --preapproval). It answers nothing else: every other command, every edit and everything on the network still asks, and a question that your hooks or your rules ask is still asked. Each use is written in the ledger as preapproved. Off: no pre-approval can be made, given to a run or used.").reach();
    /// After one approved edit, later edits of files in the same folder go through with no question until the run ends (card OA6-05, ED-10, decision D-08). Off: every edit asks, as it always did.
    EDIT_GRANT = Flag::opt_in("AQUA_EDIT_GRANT", ON_1_TRUE, "Lets you, in the app (the UI token), answer the question about the first edit of a coding run with \"allow edits in this folder for this run\" (POST /api/v1/browser/confirm with decision approve and scope run). Later edits of files in that folder and the folders inside it (fs.edit, fs.write and fs.patch that only change or create files) then go through with no question until the run ends or is stopped, and for at most two hours; the grant lives in memory only, is for that run and no other, is not given to a helper agent and cannot be widened. It never covers a delete, a move, a command, git, a secret file, a script or build file, or a name the file tools never write (.vscode, .cargo, .husky, .idea, .devcontainer...), and a question that your hooks, your rules or your autonomy level ask is still asked. It ends the moment the run reads anything from outside the project (a web page, a mail, a command's output, a file outside the project folder): every edit then asks again for the rest of the run. Reading files inside the project does not end it. Every grant, every edit it lets through and every end is written in the ledger. Off: no question offers the choice, no answer makes a grant, and every edit asks.").reach();
    /// `code.outline`, `code.symbols`, `code.deps` and `code.refs`: the symbols of a source file, where a name is defined in a folder, what a file imports and who imports it, and where a name is used (cards OA12-02 and OA12-03, tasks CU-08, TL-13 and CU-09). Off until the rounds a coding task needs have been measured with it on.
    CODE_TOOLS = Flag::opt_in("AQUA_CODE_TOOLS", ON_1_TRUE, "Offers code.outline (the symbols of one source file, each with the lines it spans and its first line, a method under its type), code.symbols (finds where a function, class or type is defined, by its name or some words of it, in the allowed folder, best match first), code.deps (what one file imports and which files import it, for Rust, TypeScript, JavaScript, Python and Go) and code.refs (the lines that use a name, tagged call, import, test or ref, by file) to a coding run, so that a long file is read by the part that is needed, a name is found without reading files and what a change reaches is seen before it is made. All four read through the same folder checks as fs.read and fs.grep, run nothing, and make the symbols and the imports from the shape of the lines (not a parser: Rust, Python, TypeScript, JavaScript and Go best, the rest by their common keywords). Off until the rounds a coding task needs have been measured with it on; with it off none of the four is on offer (they are listed as unavailable) and the coding prompt does not mention them.");
    /// `code.map`: a map of the project in about 250 tokens (card OA12-03, task CU-09). Off: the tool is not on offer; the map is in no prompt either way.
    REPO_MAP = Flag::opt_in("AQUA_REPO_MAP", ON_1_TRUE, "Offers code.map to a coding run: a map of the project in about 250 tokens (up to 1,000 when it asks), the files most worth looking at for a goal and the names in each, made from the import lines, the names of the symbols and the words of the goal, and the files the run says it works on. It is a guess from the shape of the lines, reads through the same folder checks as fs.read, runs nothing, and its file names and symbols are text from the project, shown as data. The map is a tool the run may call, and what it answers is put in no prompt: only with it on the tool is named in the coding prompt (one line), as every tool on offer is. Off until the rounds a coding task needs have been measured with it on; with it off code.map is not on offer (it is listed as unavailable) and the coding prompt does not mention it. It is separate from AQUA_CODE_TOOLS.");
    /// Code is shown, not spoken (card OA10-06, UX-08). Off: a fenced block is read aloud as it always was.
    CODE_SPEECH = Flag::opt_in("AQUA_CODE_SPEECH", ON_1_TRUE, "Leaves code out of what is spoken: a fenced block (three or more backticks or tildes) and a unified diff written outside a fence are not read aloud, and \"The code is on the screen.\" is said once in the place of the first. The written answer is not changed. Off: code is read aloud as it always was. Turning it on changes what is heard, not what the model is told.");
    /// The words of the project the person works in are given to the speech model of a dictation clip (card OA10-06, UX-16). Off until it has been measured on recorded identifiers.
    PROJECT_TERMS = Flag::opt_in("AQUA_PROJECT_TERMS", ON_1_TRUE, "Adds the words of the project you work in (file names, package and dependency names, the names the newest source files declare; never a secret-looking file, line or word; at most 200 words and 800 characters, your own words first) to the prompt that primes the speech model for a dictation clip. The voice path, whose prompt is the wake phrase, is not changed. Off until the effect on the words heard has been measured: nothing is added to the prompt.");
    /// Coding by voice: the program answers "read the diff", "next change", "line by line" and "repeat", and puts "show me the diff" and "undo the last run" on the screen as cards (card OA10-06, UX-16). Off: they are requests like any other.
    CODING_VOICE = Flag::opt_in("AQUA_CODING_VOICE", ON_1_TRUE, "Answers the spoken or typed phrases \"read the diff\", \"next change\", \"previous change\", \"line by line\" and \"repeat\" with no model, from the changes of the last run that changed files (the record of GET /api/v1/runs/{id}/diff, kept when AQUA_SNAPSHOT is on), and puts \"show me the diff\" and \"undo the last run\" on the screen as a card (GET /api/v1/coding/staged) that you press in the app: a voice never undoes anything and never approves anything. Only for your own client. Off: those phrases are requests like any other.");
    /// Model servers on this machine (an Ollama, an LM Studio) that the person lists in `data/models/providers.json` (card OA9-04, tasks PR-07 and PR-08). Off: the file is not read.
    MODEL_PROVIDERS = Flag::opt_in("AQUA_MODEL_PROVIDERS", ON_1_TRUE, "Reads data/models/providers.json, the list of model servers on this machine (an Ollama, an LM Studio, a vLLM, a llama-server of your own: http://127.0.0.1:port/v1 or localhost, in plain http, never another machine) that Aqua may ask, and lets GET /api/v1/models/providers show them and POST /api/v1/models/providers/test try one with a few words. Nothing in the agent, the chat or the flows asks such a server: the models Aqua runs are the ones it always ran. A server on another machine is read, shown with the reason and never asked.");
    /// What the file tools say about a change that leaves a file's syntax worse (card OA6-03, tasks ED-09 and CU-14). Off: nothing is looked at, as it always was. `warn`: a note after the change. `on`: also the refusal of a change that breaks a JSON file that was valid.
    SYNTAX_GATE = Flag {
        name: "AQUA_SYNTAX_GATE",
        kind: Kind::Choice { choices: &[("on", ON_1_TRUE), ("warn", &["warn"])] },
        default: "off",
        doc: "Looks at what fs.write (an overwrite), fs.edit and fs.patch leave behind. warn: a note after a change that leaves a file's brackets or quotes unbalanced (Rust, JavaScript, TypeScript, Go, C and C++, Java, C#, Kotlin, Python, TOML, CSS, JSON with comments) or breaks a JSON file; on: the same notes, and a change that turns a JSON file that parsed into one that does not is refused, nothing written, with the line. A file that was already broken is never blamed on the change, a text scan is a guess and only ever warns, and a file that is made or added to is not judged. Off: nothing is looked at, as it always was.",
        reach: false,
    };
    /// A formatter after an edit (card OA6-03, tasks ED-08 and EX-05). Off: no formatter runs after an edit.
    CODING_FORMAT = Flag::opt_in("AQUA_CODING_FORMAT", ON_1_TRUE, "Runs the project's own formatter on a file a coding run has just changed, when the project says it has one (rustfmt.toml for Rust, a ruff setting for Python, go.mod for Go, a Prettier or Biome setting with the program in node_modules for JavaScript and TypeScript), as a term.run through the tool host (so the terminal's rules ask as they ask for any program), once for each of the first three changed files, 20 seconds at most, before the check that follows the edit; says that the file was formatted and what its version is now. A formatter that is missing, refused or fails is told in one line and never fails the edit. Off: no formatter runs after an edit, as it always was.");
    /// The output of the check after an edit as a short block of errors per file (card OA6-03, task CU-10). Off: the output is shown as it was.
    DIAGNOSTICS = Flag::opt_in("AQUA_DIAGNOSTICS", ON_1_TRUE, "Shows the output of the check that follows an edit (python -m py_compile after a Python edit, cargo check or npm run typecheck with AQUA_CODING_CHECK=on) as the errors it found, one block per file with the files the run changed first (ERROR [12:5] mismatched types (E0308)), at most 20 for a file and about 900 tokens in all, warnings left out, instead of the whole text; a check that failed in a way that has no error line, or that timed out, is shown as it printed. The gate that asks for a passing check still reads the whole of it. Off: the output is shown as it always was (or as AQUA_SHORT_CHECK_OUTPUT says).");
    /// Agent definitions and helpers (card OA11-04, tasks AG-05, AG-04, EX-12, WF-17 and TL-12). Off until the rounds and the accuracy of the coding tasks have been measured with a helper.
    AGENTS = Flag::opt_in("AQUA_AGENTS", ON_1_TRUE, "Agent definitions and helpers: the built-in agents (build, plan, explore, review) and the ones you wrote in data/agents/<name>.md (or a repository whose .aqua text you trusted ships in .aqua/agents) are listed by GET /api/v1/agents, a run can be asked to be one (agent in the request of POST /api/v1/mcp/agent, agent: in a command) and then has only the tools the agent names, its words as notes after the rules and its own limit of rounds and seconds, and a coding run is offered agent.task, which hands one question to a helper that works in a context of its own (at most 3 a run, one level deep, 6 rounds and 120 seconds by default), can only read and search, cannot ask anybody, and whose answer comes back as text from outside. A helper is served by the same one model, after the run has paused for it and never beside it; what that costs the run's prompt cache has not been measured on this build. Off until the rounds and the accuracy of the coding tasks have been measured with it: with it off nothing is listed, no agent can be named, the tool is not on offer and the coding prompt does not mention it.");
    /// MCP servers on another machine, reached over HTTP (card OA11-03, task EX-11). Off: an `url` entry of `mcp.json` is not started. It widens what the agent can reach, so a promotion may not set it.
    MCP_REMOTE = Flag::opt_in("AQUA_MCP_REMOTE", ON_1_TRUE, "Lets a server in data/tools/mcp.json be a remote one (\"url\": \"https://...\") instead of a program on this computer. It is spoken to over HTTP through the tool host's own HTTP engine, so the allow-list of data/tools/http.json (the host must be on it, there is no question for an unlisted one), the guard against private addresses, no redirects and a cap of 16 MB on a response all apply; the headers it is sent are plain text or the name of a secret in the credential store (a header that carries a credential must be a secret); the server is trusted by its address and header names like any other, its tools ask every time and no answer is kept for the session. Not built: logging in with OAuth, and the stateless protocol of 2026-07-28 over HTTP.").reach();
    /// Reading the config files again while Aqua runs (card OA11-03, task EX-13). Off: only the route (`POST /api/v1/config/reload`) reads them again.
    CONFIG_WATCH = Flag::opt_in("AQUA_CONFIG_WATCH", ON_1_TRUE, "Looks every two seconds at data/hooks.json, data/tools/mcp.json and the SKILL.md files of data/skills, and reads one again when it changed, with the rules of POST /api/v1/config/reload: only what cannot loosen anything is applied while Aqua runs (a hook that was running and is gone, or changed, waits for a restart; a server that changed or is new is not trusted), and nothing is read again while a run is going on.");
    /// The development skills that ship with Aqua (card OA12-01, tasks WF-15, WF-16 and WF-18). Off until a benchmark on the model shows that the lists in the prompt cost no accuracy.
    SHIPPED_SKILLS = Flag::opt_in("AQUA_SHIPPED_SKILLS", ON_1_TRUE, "Offers the ten development skills that ship with Aqua (fix-failing-tests, debug-build, review-diff, write-tests, add-endpoint, db-migration, upgrade-deps, release-notes, incident-triage, init-project) next to the ones you wrote: a goal they fit lists them in the prompt (a name and one line each, marked built in) and skill.load reads them. Each one only narrows what the run may use, and none reaches the network or runs an installer; a skill of yours or of a trusted repository with the same name wins. Off until a benchmark on the model shows the lists cost no accuracy; with it off they are not in the prompt, skill.load does not know them, and aqua-cli doctor names them.").reach();
    /// Worktrees for a coding run (card OA7-05, task TM-13). Off until a coding run has been measured working in one.
    WORKTREES = Flag::opt_in("AQUA_WORKTREES", ON_1_TRUE, "Offers git.worktree_add, git.worktree_list and git.worktree_remove: a coding run can make a second working folder of a repository, on a branch of its own, under .aqua/worktrees inside the repository's own folder (a folder Aqua owns, with a .gitignore of its own, which must be in a folder Aqua may write), work there through the git tools (its repo alias), fs.* and the cwd of term.run, and leave your first folder alone. Making one always asks and says about how many files and megabytes it copies; removing one always asks, is by name, only for a worktree Aqua made and recorded (data/tools/git_worktrees.json), refuses one with changes unless force=true (changes to tracked files are kept in a backup reference, untracked files are lost), takes away a link or junction inside it on its own first, and never deletes a branch that is not merged. The worktree is not trusted: a commit there asks. Nothing is reset, cleaned, force-pushed or rewritten. Off: the tools are not on offer and nothing is read or made.").reach();
    /// Conflicts and the text of a pull request (card OA7-05, tasks TM-17 and TM-18). Off until a coding run has been measured resolving one.
    GIT_REVIEW = Flag::opt_in("AQUA_GIT_REVIEW", ON_1_TRUE, "Offers git.conflicts (the files with merge conflicts, how each is conflicted, the hunks of each text file with what each side has, and whether a merge, cherry-pick, revert or rebase is in progress), git.resolve_conflict (replaces the hunks of one file by ours, theirs, both or text the model wrote: you are asked each time and shown the change as a diff, the file is kept in the undo store first, nothing is staged or committed, and a file that changed after you were shown the diff is not written) and git.pr_draft (a title and a description for a pull request, made from the local commit log and the names of the changed files; nothing is sent, no forge is asked). With it on a commit is also refused while a file of the index is unmerged or the lines it adds hold a whole conflict (<<<<<<<, =======, >>>>>>>), and a merge that stops on conflicts names git.conflicts. Off: the tools are not on offer and a commit is what it was.").reach();
    /// `fs.edit_lines`: replace a range of lines of a file, held to an anchor line (card OA6-02, task ED-07). A prototype that is measured before it is taught: off until the edit-format bake-off has been run on the model.
    EDIT_LINES = Flag::opt_in("AQUA_EDIT_LINES", ON_1_TRUE, "Offers fs.edit_lines to a coding run: it replaces the lines start to end of a file with new text (an empty text takes them out, end = start - 1 inserts before start) and is held to an anchor, the text of line start (expect_first_line): if the file has other text there, because the lines moved or the file changed, nothing is changed and the real lines are shown so the call can be repeated. It goes through the same door as fs.edit: the same folder checks, the same question and the same diff of the change, the same undo, and a file keeps its line endings and its last line break. It is not covered by the run-scoped edit grant (AQUA_EDIT_GRANT). Off until the edit-format bake-off (documentation/EDIT_BAKEOFF.md) has been run on the model: with it off the tool is not on offer (it is listed as unavailable) and the coding prompt does not mention it.").reach();
    /// The language-server bridge (card OA12-04, tasks CU-15 and WF-20 of the OpenCode audit): the four `code.*` tools of a language server the person named in `data/tools/lsp.json`. It starts a program, so it is a reach flag (a promotion may not set it), and it stays off until the person has tried one server on a second core.
    LSP = Flag::opt_in("AQUA_LSP", ON_1_TRUE, "Offers code.definition, code.references, code.hover and code.document_symbols to a coding run: exact answers from a language server (rust-analyzer, gopls ...) that you named in data/tools/lsp.json with the absolute path of its program, its arguments and the SHA-256 of that program. A program is started only for a language the file names, in a folder marked trusted in fs.json, and only if its bytes have that hash (nothing is downloaded or installed, and a program with no hash is never started). The first call of a run for a language starts it and asks you, naming the program and its hash; it is put in a Job Object that ends the whole tree when the run ends, when it has been idle for five minutes or when it stops answering (the language is then unavailable for the rest of the run), and an answer larger than a cap and more requests than a limit are refused. What it answers is text from outside, shown as data, cut to a few lines, and a place outside the project is counted and never shown. It is read when Aqua starts: with it off (the default) the four tools are not registered at all, no list or prompt names them and no process can start. Tried only against a scripted fake server, never against a real one: see documentation/LSP_BRIDGE.md.").reach();
    /// `--cache-reuse N` of the model server (card OA9-06, task PR-13). Off: the server is started without it, as it always was.
    LLAMA_CACHE_REUSE = Flag {
        name: "AQUA_LLAMA_CACHE_REUSE",
        kind: Kind::Choice { choices: &[("64", &[]), ("128", &[]), ("256", &[]), ("512", &[])] },
        default: "off",
        doc: "Starts the model server with --cache-reuse N: the smallest piece of a prompt, in tokens, that the server will take from its cache by shifting it, even when the text in front of it changed. Only these values are passed on; anything else is ignored with a line in the log. A speed experiment (card OA9-06): off until the speed gate and the planner bake-off have been run with it on, and a server that does not list the option is started as before.",
        reach: true,
    };
    /// `--cache-ram N` of the model server (card OA9-06, task PR-13). Off: the server keeps its own default.
    LLAMA_CACHE_RAM = Flag {
        name: "AQUA_LLAMA_CACHE_RAM",
        kind: Kind::Choice { choices: &[("0", &[]), ("512", &[]), ("1024", &[]), ("2048", &[]), ("4096", &[])] },
        default: "off",
        doc: "Starts the model server with --cache-ram N: how many MiB of the computer's memory the server may use to keep the prompts of idle slots (0 keeps none; the server's own default is 8,192). Only these values are passed on. A speed and memory experiment (card OA9-06): off until it has been measured, and a server that does not list the option is started as before.",
        reach: true,
    };
    /// A folder for the saved state of a slot of the model server, `--slot-save-path` (card OA9-06, tasks PR-13 and CX-18). Off: no folder is given and nothing is saved.
    LLAMA_SLOT_SAVE = Flag::opt_in("AQUA_LLAMA_SLOT_SAVE", ON_1_TRUE, "Starts the model server with --slot-save-path data/cache/kv, a folder of Aqua's own under the data folder that holds nothing but the saved state of a slot, saves the slot of the agent's prompt when the server is stopped on purpose or goes idle (only when the start of the agent's prompt is in it) and puts it back right after the next start of the server (only when a file for this model file and this prompt start is there). A file there holds the model's state for that conversation start: the tokens of a conversation and what the model made of them, so it is as private as the conversation itself; its name holds hashes and no text. A build without the option, a refused request or a damaged file is one line of the log and never delays or fails a request. The folder is kept to 300 MB (the oldest file goes first), and a function that empties it exists but no command of Aqua calls it yet. A speed experiment (card OA9-06), tried only against a fake model server: off until the first request after a start has been measured with a restored slot.").reach();
    /// A small draft model for speculative decoding, `--spec-draft-model` and its tuning (card OA9-06, task PR-12). Off: the server is started without one, as it always was.
    LLAMA_DRAFT = Flag {
        name: "AQUA_LLAMA_DRAFT",
        kind: Kind::Choice { choices: &[("cpu", ON_1_TRUE), ("gpu", &[])] },
        default: "off",
        doc: "Starts the model server with a small draft model for speculative decoding: the file named by AQUA_LLAMA_DRAFT_MODEL (a .gguf file name in the model folder, or a full path inside it or inside the one folder AQUA_LLAMA_DRAFT_FOLDER allows; Aqua never downloads one) with at most 5 tokens guessed at a time, at least 1 and none below a probability of 0.5. cpu (also on, 1, true) keeps the draft on the processor with --spec-draft-ngl 0, since the card is already full with the main model; gpu leaves it to the server. A file that is missing, is not a GGUF file, is the main model, has another vocabulary or is outside the folders leaves the launch as it was and the status says why. A speed experiment (card OA9-06): off until a measured gain in tokens a second over at least three runs, and the share of guesses the server kept (in the status) is 55 percent or more.",
        reach: true,
    };
    /// How the planner's tool calls reach the model: the constrained JSON of today, or the server's own tool calls (card OA9-06, task PR-14, decision D-17). json until a bake-off says native is as accurate.
    TOOL_MODE = Flag {
        name: "AQUA_PLANNER_TOOL_MODE",
        kind: Kind::Choice { choices: &[("native", &[])] },
        default: "json",
        doc: "How the agent's planner gets its tool calls from the local model. json (the default, and what any other word means) holds the reply to a JSON schema, as it always was. native sends the tools of the round to the model server as its own tool list and turns the tool calls of the reply into the same call the JSON gives, which then goes through the same door with the same checks: a tool that was not offered, bad arguments or a call that is not a call are refused, never run and never guessed at. A model's entry in data/models/profiles.json may say tool_mode native or json for that model (read with AQUA_MODEL_PROFILES on, and it wins over this switch). The agent's planner only. A speed and accuracy experiment (card OA9-06): off until a bake-off says native calls are at least as accurate as the constrained JSON.",
        reach: true,
    };
    /// A smaller second model for the roles decision, fast and diagnosis, with a fall back to the main model (card OA9-06, task PR-11). Off: the main model answers every role, as it always did.
    FAST_TIER = Flag::opt_in("AQUA_FAST_TIER", ON_1_TRUE, "Names a smaller local model (the .gguf file AQUA_FAST_TIER_MODEL, in the model folder; Aqua never downloads one) for the roles decision, fast and diagnosis, with a fall back to the main model when the small one is missing, cannot start, is not ready yet, takes longer than AQUA_FAST_TIER_SECS, answers with an error or has too small a window. The small model runs in a second model server of its own, started at the first call that needs it (never when Aqua starts), on the processor only unless AQUA_FAST_TIER_NGL says otherwise (so it never takes video memory from the main model), with a small window (AQUA_FAST_TIER_CTX), one slot, no draft model and no saved slots; it listens on 127.0.0.1 only, is stopped after AQUA_FAST_TIER_IDLE_SECS without a call and when Aqua stops, and a start that fails is not tried again for five minutes. Routed: the engine's decision calls and the rewrites of the dictation (role fast); no part of the engine asks for the role diagnosis yet. The status (GET /api/v1/models/roles, key speed_experiments) says whether it is off, waiting, starting, ready or failed, and why. A speed experiment (card OA9-06), tried only against a fake model server: off until a bake-off says the small model is accurate enough (scripts/eval/speed_experiments/run_experiment.py fast_tier).").reach();
    /// How long the small model may take before the main model is asked (card OA9-06). Used only with the fast tier on.
    FAST_TIER_SECS = Flag::int("AQUA_FAST_TIER_SECS", 1, 600, "20", "How many seconds the small model of the fast tier (AQUA_FAST_TIER) may take, of the call's own time, before the main model is asked instead; the main model then has what is left of the call's time. From 1 to 600; 20 is the default. It applies only while the fast tier is on. A call that finds the small model's server still loading is not waited for: it goes to the main model at once.");
    /// How many layers of the small model go to the graphics card (card OA9-06). 0, the processor only, unless the person says otherwise.
    FAST_TIER_NGL = Flag::int("AQUA_FAST_TIER_NGL", 0, 99, "0", "How many layers of the small model of the fast tier (AQUA_FAST_TIER) go to the graphics card. 0 is the default: the small model runs on the processor and never takes video memory from the main model, which on a 4 GB card is full with its 22 layers. A number above 0 starts the second server with the graphics-card build and that many layers, and is the person's choice and risk: it was not measured, and a card that holds both models is not assumed. From 0 to 99.");
    /// The window of the small model's server (card OA9-06).
    FAST_TIER_CTX = Flag::int("AQUA_FAST_TIER_CTX", 1024, 16384, "4096", "The window, in tokens, of the second model server of the fast tier (AQUA_FAST_TIER): one slot of this size, so a small model keeps a small cache. A prompt that does not fit it is answered by the main model instead. From 1024 to 16384; 4096 is the default.");
    /// How long the small model's server may sit unused before it is stopped (card OA9-06).
    FAST_TIER_IDLE_SECS = Flag::int("AQUA_FAST_TIER_IDLE_SECS", 10, 86400, "300", "How many seconds the second model server of the fast tier (AQUA_FAST_TIER) may sit without a call before it is stopped to give its memory back; the next call that needs it starts it again. From 10 to 86400; 300 is the default.");
}

thread_local! {
    /// The values a test has set for this thread: (variable, value). `None` is "not set".
    static PINNED: RefCell<Vec<(String, Option<String>)>> = const { RefCell::new(Vec::new()) };
}

/// Puts the pinned values back when the closure of [`with_overrides`] ends, whatever way it ends.
struct Unpin(usize);

impl Drop for Unpin {
    fn drop(&mut self) {
        PINNED.with(|p| p.borrow_mut().truncate(self.0));
    }
}

/// Runs `f` with these values for the flags **on this thread** (`None`: the variable is not set), whatever the process environment says. For tests: it changes nothing for any other
/// thread, so tests that use it cannot disturb each other the way `std::env::set_var` does. Code that runs the flag's reader on another thread does not see it.
pub fn with_overrides<R>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
    let mark = PINNED.with(|p| p.borrow().len());
    PINNED.with(|p| p.borrow_mut().extend(pairs.iter().map(|(n, v)| (n.to_string(), v.map(str::to_string)))));
    let _unpin = Unpin(mark);
    f()
}

/// Where flag values come from.
pub struct Flags {
    /// `None`: this thread's pinned values, then the process environment. `Some`: exactly these (a test's own).
    own: Option<BTreeMap<String, String>>,
}

static GLOBAL: Flags = Flags { own: None };

impl Flags {
    /// The process: the environment (where the core put the promoted flags at start), unless a test pinned a value for this thread.
    pub fn global() -> &'static Flags {
        &GLOBAL
    }

    /// Flags with exactly these values and no look at the environment, so a test needs no `set_var`.
    pub fn for_test(pairs: &[(&str, &str)]) -> Flags {
        Flags { own: Some(pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()) }
    }

    /// The text of the variable, or `None` when it is not set.
    pub fn raw(&self, flag: &Flag) -> Option<String> {
        if let Some(own) = &self.own {
            return own.get(flag.name).cloned();
        }
        if let Some(pinned) = PINNED.with(|p| p.borrow().iter().rev().find(|(n, _)| n == flag.name).map(|(_, v)| v.clone())) {
            return pinned;
        }
        std::env::var(flag.name).ok()
    }

    pub fn is_on(&self, flag: &Flag) -> bool {
        flag.parse_switch(self.raw(flag).as_deref())
    }

    pub fn choice(&self, flag: &Flag) -> &'static str {
        flag.parse_choice(self.raw(flag).as_deref())
    }

    pub fn int(&self, flag: &Flag) -> i64 {
        flag.parse_int(self.raw(flag).as_deref())
    }
}

/// The text of the variable (what each caller used to get from `std::env::var(..).ok()`).
pub fn raw(flag: &Flag) -> Option<String> {
    Flags::global().raw(flag)
}

/// Is this switch on?
pub fn is_on(flag: &Flag) -> bool {
    Flags::global().is_on(flag)
}

/// The choice this flag has now.
pub fn choice(flag: &Flag) -> &'static str {
    Flags::global().choice(flag)
}

/// The number this flag has now.
pub fn int(flag: &Flag) -> i64 {
    Flags::global().int(flag)
}

/// One flag as `GET /api/v1/flags` tells it.
pub fn views() -> Vec<serde_json::Value> {
    let flags = Flags::global();
    ALL.iter()
        .map(|f| {
            let raw = flags.raw(f);
            serde_json::json!({
                "name": f.name,
                "kind": f.kind_word(),
                "default": f.default,
                // What the flag is now, as the code reads it: never the text of the variable, which is whatever someone typed.
                "value": f.value_of(raw.as_deref()),
                "set": raw.is_some(),
                "values": f.spellings(),
                "doc": f.doc,
                "reach": f.reach,
                "promotable": crate::promoted::ALLOWED.iter().any(|(n, _)| *n == f.name),
            })
        })
        .collect()
}

/// The table for `CONFIGURATION.md`, from [`ALL`].
pub fn markdown_table() -> String {
    let mut s = String::from("| Variable | Kind | Default | Values | What it does |\n|---|---|---|---|---|\n");
    for f in ALL {
        let default = if f.default.is_empty() { "not set".to_string() } else { format!("`{}`", f.default) };
        let reach = if f.reach { " **It widens what the agent can reach, so a promotion may not set it.**" } else { "" };
        s.push_str(&format!("| `{}` | {} | {} | {} | {}{} |\n", f.name, f.kind_word(), default, f.spellings(), f.doc, reach));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values the old readers were given in the wild and the ones that might be: unset, empty, spaces, each spelling in each case, and near misses.
    fn matrix() -> Vec<Option<&'static str>> {
        let mut v: Vec<Option<&'static str>> = vec![None];
        v.extend(["", " ", "on", "ON", " On ", "oN", "1", "true", "TRUE", " true", "yes", "YES", "off", "OFF", "Off", " off ", "0", "false", "False", "no", "NO", "2", "enabled", "onn", "of", "shadow", "-1"].map(Some));
        v
    }

    // The readers as they were written before this card, copied here so that the new accessor is held to them.
    fn old_tool_search(v: Option<&str>) -> bool {
        matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }
    fn old_router_v2(v: Option<&str>) -> bool {
        !matches!(v.map(str::trim), Some("0" | "false" | "off"))
    }
    fn old_research(v: Option<&str>) -> bool {
        matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true" | "yes"))
    }
    fn old_skill_proposals(v: Option<&str>) -> bool {
        v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "1" | "true" | "yes"))
    }
    fn old_clipboard(v: Option<&str>) -> bool {
        matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true" | "yes"))
    }
    fn old_screen(v: Option<&str>) -> bool {
        !matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("0" | "off" | "false" | "no"))
    }
    fn old_recipes_v2(v: Option<&str>) -> bool {
        matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }
    fn old_plan(v: Option<&str>) -> bool {
        matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }
    fn old_reader(v: Option<&str>) -> bool {
        matches!(v.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }

    #[test]
    fn each_switch_reads_a_value_exactly_as_its_old_reader_did() {
        let pairs: [(&Flag, fn(Option<&str>) -> bool); 9] = [
            (&TOOL_SEARCH, old_tool_search),
            (&ROUTER_V2, old_router_v2),
            (&RESEARCH, old_research),
            (&SKILL_PROPOSALS, old_skill_proposals),
            (&CLIPBOARD_TOOLS, old_clipboard),
            (&SCREEN_TOOLS, old_screen),
            (&RECIPES_V2, old_recipes_v2),
            (&PLAN, old_plan),
            (&READER, old_reader),
        ];
        for (flag, old) in pairs {
            for value in matrix() {
                assert_eq!(flag.parse_switch(value), old(value), "{} = {value:?}", flag.name);
                // And through the accessor, with a value for a test and with a value pinned for the thread.
                let test = Flags::for_test(&value.map(|v| vec![(flag.name, v)]).unwrap_or_default());
                assert_eq!(test.is_on(flag), old(value), "{} = {value:?} (for_test)", flag.name);
                assert_eq!(with_overrides(&[(flag.name, value)], || is_on(flag)), old(value), "{} = {value:?} (pinned)", flag.name);
            }
        }
    }

    #[test]
    fn the_door_reads_a_value_as_the_comparison_always_did() {
        // `DoorMode::parse` is the reader (it also reads the older variable); the flag's choice must agree with it for AQUA_DOOR alone.
        use crate::door_shadow::DoorMode;
        for value in matrix() {
            let mode = DoorMode::parse(value, None);
            let want = match mode {
                DoorMode::Off => "off",
                DoorMode::On => "on",
                DoorMode::Shadow => "shadow",
            };
            assert_eq!(DOOR.parse_choice(value), want, "AQUA_DOOR = {value:?}");
            let legacy_off = matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("off" | "0" | "false"));
            assert_eq!(ONE_DOOR.parse_choice(value) == "off", legacy_off, "AQUA_ONE_DOOR = {value:?}");
        }
        assert_eq!(with_overrides(&[("AQUA_DOOR", Some(" ON ")), ("AQUA_ONE_DOOR", None)], DoorMode::from_env), DoorMode::On);
        assert_eq!(with_overrides(&[("AQUA_DOOR", Some("on")), ("AQUA_ONE_DOOR", Some("off"))], DoorMode::from_env), DoorMode::Off, "either one saying off turns the comparison off");
        assert_eq!(with_overrides(&[("AQUA_DOOR", None), ("AQUA_ONE_DOOR", None)], DoorMode::from_env), DoorMode::Shadow);
    }

    #[test]
    fn a_default_is_what_the_old_code_did_when_the_variable_was_not_set() {
        assert_eq!(
            ALL.iter().map(|f| (f.name, f.value_of(None))).collect::<Vec<_>>(),
            vec![
                ("AQUA_TOOL_SEARCH", "off".to_string()),
                ("AQUA_DOOR", "shadow".to_string()),
                ("AQUA_ONE_DOOR", "".to_string()),
                ("AQUA_ROUTER_V2", "on".to_string()),
                ("AQUA_RESEARCH", "off".to_string()),
                ("AQUA_SKILL_PROPOSALS", "off".to_string()),
                ("AQUA_CLIPBOARD_TOOLS", "off".to_string()),
                ("AQUA_SCREEN_TOOLS", "on".to_string()),
                ("AQUA_RECIPES_V2", "off".to_string()),
                ("AQUA_PLAN", "off".to_string()),
                ("AQUA_READER", "off".to_string()),
                // (card OA5-02) a new flag, added to the end of the table: on unless it says no.
                ("AQUA_TOOL_OUTPUT_SPILL", "on".to_string()),
                // (card OA9-01) off: the engine asks the server directly unless this says on.
                ("AQUA_MODEL_ROUTER", "off".to_string()),
                // (card OA5-01) two new flags, off: the tools of the loop itself, and one more pack for a coding goal.
                ("AQUA_AGENT_TOOLS", "off".to_string()),
                ("AQUA_CODING_PACKS", "off".to_string()),
                // (card OA4-02) off: a coding goal's prompt is ordered as it always was unless this says on.
                ("AQUA_PREFIX_STABLE", "off".to_string()),
                // (card OA11-01) two new flags, off: a repository's .aqua pack, and the other files of a skill.
                ("AQUA_PROJECT_PACK", "off".to_string()),
                ("AQUA_SKILLS_V2", "off".to_string()),
                // (card OA5-04) off: the last round of a budget is free to call a tool unless this says on.
                ("AQUA_LAST_STEP_FINAL", "off".to_string()),
                // (card OA5-03) off: the tool fs.outline does not exist unless this says on.
                ("AQUA_FS_OUTLINE", "off".to_string()),
                // (card OA5-03) off: a result that a later one repeats is shown in full unless this says on.
                ("AQUA_COLLAPSE_REPEATS", "off".to_string()),
                // (card OA9-03) two new switches, off, and a seed that is not set.
                ("AQUA_MODEL_PROFILES", "off".to_string()),
                ("AQUA_SAMPLING_PRESETS", "off".to_string()),
                ("AQUA_SEED", "-1".to_string()),
                // (card OA9-02) two new numbers, 0 (off): how long a streamed answer may be quiet, and how long a stream may wait for its first piece.
                ("AQUA_STREAM_IDLE_SECS", "0".to_string()),
                ("AQUA_STREAM_FIRST_EVENT_SECS", "0".to_string()),
                // (card OA4-05) two new switches, off: the tools project.profile and check.run, and the notes of the folders a file is read from.
                ("AQUA_CHECK_TOOLS", "off".to_string()),
                ("AQUA_NESTED_NOTES", "off".to_string()),
                // (card OA10-03) three new switches, off: words typed with a "no" let a coding run go on, "also ..." and "then ..." are said to the running task, and a run that needs you or has ended is announced.
                ("AQUA_DENY_FEEDBACK", "off".to_string()),
                ("AQUA_STEER_CUES", "off".to_string()),
                ("AQUA_ATTENTION", "off".to_string()),
                // (card OA11-02) two new switches, off: custom commands, and the command tools of a file or a trusted repository.
                ("AQUA_COMMANDS", "off".to_string()),
                ("AQUA_COMMAND_TOOLS", "off".to_string()),
                // (card OA4-04) off: the answer of a coding run is the model's words only unless this says on.
                ("AQUA_ANSWER_EVIDENCE", "off".to_string()),
                // (card OA4-04) off: the coding record keeps the first error for ever, the check after an edit is no step of the run, and the model sees the whole output of that check, unless these say on.
                ("AQUA_CODING_RECORD", "off".to_string()),
                ("AQUA_CHECK_STEPS", "off".to_string()),
                ("AQUA_SHORT_CHECK_OUTPUT", "off".to_string()),
                // (card OA7-03) off: no checkpoint of a project is taken around a run unless this says on.
                ("AQUA_SNAPSHOT", "off".to_string()),
                // (card OA10-04) off: nothing is kept of a coding session, no run is given more rounds and no session summary is made unless these say on.
                ("AQUA_CODING_SESSIONS", "off".to_string()),
                ("AQUA_CONTINUE", "off".to_string()),
                ("AQUA_SESSION_SUMMARY", "off".to_string()),
                // (card OA10-05) off: nothing the person pre-approves answers a question unless this says on.
                ("AQUA_PREAPPROVALS", "off".to_string()),
                // (card OA6-05) off: no question offers the run option and no edit goes through on a grant unless this says on.
                ("AQUA_EDIT_GRANT", "off".to_string()),
                // (card OA12-02, OA12-03) off: the tools code.outline, code.symbols, code.deps and code.refs are not on offer unless this says on, and code.map not unless the next says on.
                ("AQUA_CODE_TOOLS", "off".to_string()),
                ("AQUA_REPO_MAP", "off".to_string()),
                // (card OA10-06) off: code is read aloud, the words of a project are not given to the speech model, and the coding phrases are requests like any other, unless these say on.
                ("AQUA_CODE_SPEECH", "off".to_string()),
                ("AQUA_PROJECT_TERMS", "off".to_string()),
                ("AQUA_CODING_VOICE", "off".to_string()),
                // (card OA9-04) off: data/models/providers.json is not read unless this says on.
                ("AQUA_MODEL_PROVIDERS", "off".to_string()),
                // (card OA6-03) off: a change to a file is made as it always was (nothing looked at), no formatter runs after an edit, and the output of the check after an edit is not made a diagnostics block.
                ("AQUA_SYNTAX_GATE", "off".to_string()),
                ("AQUA_CODING_FORMAT", "off".to_string()),
                ("AQUA_DIAGNOSTICS", "off".to_string()),
                // (card OA11-04) off: no agent can be named, no agent is listed, and a coding run is not offered a helper (agent.task) unless this says on.
                ("AQUA_AGENTS", "off".to_string()),
                // (card OA11-03) off: an url entry of mcp.json is not started, and the config files are read again only by the route, unless these say on.
                ("AQUA_MCP_REMOTE", "off".to_string()),
                ("AQUA_CONFIG_WATCH", "off".to_string()),
                // (card OA12-01) off: the ten development skills that ship with Aqua are not on offer unless this says on.
                ("AQUA_SHIPPED_SKILLS", "off".to_string()),
                // (card OA7-05) off: no worktree tool is on offer, and the conflict tools, the pull request draft and the refusal of conflict markers in a commit are not there, unless these say on.
                ("AQUA_WORKTREES", "off".to_string()),
                ("AQUA_GIT_REVIEW", "off".to_string()),
                // (card OA6-02) off: the tool fs.edit_lines is not on offer unless this says on.
                ("AQUA_EDIT_LINES", "off".to_string()),
                // (card OA12-04) off: the language-server tools are not registered, and no language server is started, unless this says on.
                ("AQUA_LSP", "off".to_string()),
                // (card OA9-06) off: the model server is started without --cache-reuse, without --cache-ram and without a folder for saved slots unless these say so.
                ("AQUA_LLAMA_CACHE_REUSE", "off".to_string()),
                ("AQUA_LLAMA_CACHE_RAM", "off".to_string()),
                ("AQUA_LLAMA_SLOT_SAVE", "off".to_string()),
                // (card OA9-06) off: the model server is started without a draft model unless this says cpu or gpu.
                ("AQUA_LLAMA_DRAFT", "off".to_string()),
                // (card OA9-06) json: the planner's tool calls are held to a schema, as they always were, unless this says native.
                ("AQUA_PLANNER_TOOL_MODE", "json".to_string()),
                // (card OA9-06) off, and 20 seconds, the processor only, a window of 4096 and 300 seconds before an unused second server is stopped: the fast tier, and a promotion may not set the switch.
                ("AQUA_FAST_TIER", "off".to_string()),
                ("AQUA_FAST_TIER_SECS", "20".to_string()),
                ("AQUA_FAST_TIER_NGL", "0".to_string()),
                ("AQUA_FAST_TIER_CTX", "4096".to_string()),
                ("AQUA_FAST_TIER_IDLE_SECS", "300".to_string()),
            ]
        );
        for f in ALL {
            // The default shown is the one the kind gives.
            match f.kind {
                Kind::OptIn { .. } => assert_eq!(f.default, "off", "{}", f.name),
                Kind::OptOut { .. } => assert_eq!(f.default, "on", "{}", f.name),
                // A choice that is none of the named ones is the default, so the default may be a word of its own (`shadow`) or empty.
                Kind::Choice { choices } => assert!(!choices.iter().any(|(n, _)| *n == f.default) || f.default.is_empty(), "{}: the default is not one of the named choices", f.name),
                Kind::Int { .. } => assert!(f.default.parse::<i64>().is_ok(), "{}", f.name),
            }
        }
    }

    #[test]
    fn the_table_is_complete_unique_and_documented() {
        let mut names: Vec<&str> = ALL.iter().map(|f| f.name).collect();
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count, "a name twice");
        for f in ALL {
            assert!(f.name.starts_with("AQUA_") && f.name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'), "{}", f.name);
            assert!(f.doc.len() > 20 && f.doc.ends_with('.'), "{}: say what it does, in a sentence", f.name);
        }
        let table = markdown_table();
        for f in ALL {
            assert!(table.contains(&format!("| `{}` |", f.name)));
        }
        assert_eq!(views().len(), ALL.len());
    }

    #[test]
    fn a_flag_that_widens_what_the_agent_reaches_is_never_something_a_promotion_may_set() {
        for f in ALL.iter().filter(|f| f.reach) {
            assert!(!crate::promoted::ALLOWED.iter().any(|(n, _)| *n == f.name), "{} widens what the agent can reach, so it stays out of promoted::ALLOWED", f.name);
        }
        // The ones that do reach, by name: a flag added to the table without the mark is caught by the review of this list.
        let reach: Vec<&str> = ALL.iter().filter(|f| f.reach).map(|f| f.name).collect();
        assert_eq!(reach, vec!["AQUA_TOOL_SEARCH", "AQUA_RESEARCH", "AQUA_CLIPBOARD_TOOLS", "AQUA_SCREEN_TOOLS", "AQUA_CODING_PACKS", "AQUA_PROJECT_PACK", "AQUA_SKILLS_V2", "AQUA_CHECK_TOOLS", "AQUA_COMMAND_TOOLS", "AQUA_PREAPPROVALS", "AQUA_EDIT_GRANT", "AQUA_MCP_REMOTE", "AQUA_SHIPPED_SKILLS", "AQUA_WORKTREES", "AQUA_GIT_REVIEW", "AQUA_EDIT_LINES", "AQUA_LSP", "AQUA_LLAMA_CACHE_REUSE", "AQUA_LLAMA_CACHE_RAM", "AQUA_LLAMA_SLOT_SAVE", "AQUA_LLAMA_DRAFT", "AQUA_PLANNER_TOOL_MODE", "AQUA_FAST_TIER"]);
        // And the two flags of the table that a promotion may set are the ones `ALLOWED` names.
        let promotable: Vec<&str> = ALL.iter().filter(|f| crate::promoted::ALLOWED.iter().any(|(n, _)| *n == f.name)).map(|f| f.name).collect();
        assert_eq!(promotable, vec!["AQUA_PLAN", "AQUA_READER"]);
    }

    #[test]
    fn a_test_value_does_not_touch_the_environment_or_another_thread() {
        let name = PLAN.name;
        let before = std::env::var(name).ok();
        assert!(with_overrides(&[(name, Some("on"))], || is_on(&PLAN)));
        assert_eq!(std::env::var(name).ok(), before, "the process environment is as it was");
        // Another thread, at the same moment, sees its own value.
        let other = with_overrides(&[(name, Some("on"))], || std::thread::spawn(move || with_overrides(&[(name, Some("off"))], || is_on(&PLAN))).join().unwrap());
        assert!(!other);
        // Nested values: the inner one wins, and the outer one comes back.
        with_overrides(&[(name, Some("on"))], || {
            assert!(with_overrides(&[(name, Some("off"))], || !is_on(&PLAN)));
            assert!(is_on(&PLAN));
        });
        // A pinned "not set" hides a variable that is set.
        let _ = with_overrides(&[(name, None)], || assert_eq!(raw(&PLAN), None));
        // `Flags::for_test` never looks at the environment at all.
        assert!(!Flags::for_test(&[]).is_on(&READER));
        assert!(Flags::for_test(&[("AQUA_READER", "on")]).is_on(&READER));
        // A panic in the closure still puts the values back.
        let _ = std::panic::catch_unwind(|| with_overrides(&[(PLAN.name, Some("on"))], || panic!("boom")));
        assert_eq!(PINNED.with(|p| p.borrow().len()), 0);
    }

    #[test]
    fn numbers_and_choices_fall_back_to_their_default_when_the_text_is_not_one_of_them() {
        let batch = Flag { name: "AQUA_TEST_BATCH", kind: Kind::Int { min: 1, max: 3 }, default: "2", doc: "A test number.", reach: false };
        for (value, want) in [(None, 2), (Some("3"), 3), (Some(" 1 "), 1), (Some("0"), 2), (Some("4"), 2), (Some("many"), 2), (Some(""), 2)] {
            assert_eq!(batch.parse_int(value), want, "{value:?}");
        }
        assert_eq!(batch.value_of(Some("3")), "3");
        assert_eq!(batch.spellings(), "a whole number from 1 to 3");
        assert_eq!(DOOR.spellings(), "on (on, 1, true); off (off, 0, false)");
    }

    #[test]
    fn the_flags_are_read_nowhere_else_with_the_environment() {
        // Acceptance of task EX-19: none of the flags of the registry is read with `std::env::var` outside this file. (Writing one, as a test does with `set_var`, is not reading it.)
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if !matches!(e.file_name().to_string_lossy().as_ref(), "target" | "node_modules" | "vendor" | ".git") {
                        rs_files(&p, out);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for top in ["core/src", "adapters", "apps/aqua-cli/src", "learning"] {
            rs_files(&root.join(top), &mut files);
        }
        let mut found = Vec::new();
        for f in files.iter().filter(|f| !f.ends_with("core/src/flags.rs") && !f.ends_with("core\\src\\flags.rs")) {
            let text = std::fs::read_to_string(f).unwrap_or_default();
            for flag in ALL {
                for reader in ["env::var(", "env::var_os("] {
                    if text.contains(&format!("{reader}\"{}\"", flag.name)) {
                        found.push(format!("{} reads {} with {reader}", f.display(), flag.name));
                    }
                }
            }
        }
        assert!(found.is_empty(), "read these through crate::flags instead: {found:?}");
    }
}
