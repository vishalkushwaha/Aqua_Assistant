//! The agent definitions that ship with Aqua (card OA11-04): `build`, `plan`, `explore` and `review`. Their names are reserved: a file of the person's or of a repository cannot take one.

use super::{AgentDef, Mode, Source};

/// The tools that only look. What the `explore` helper may call, and what a helper definition that names no tools is held to. `child_may_call` in the agent loop reads this list too, so that
/// there is one. A pattern that names a tool this build does not offer (`code.outline` without `AQUA_CODE_TOOLS`) offers nothing. (Card OA12-03: `code.deps`, `code.refs` and `code.map` only
/// read too, and are on offer under their own switches.)
pub const READ_TOOLS: &[&str] = &["fs.read", "fs.grep", "fs.list", "fs.find", "fs.stat", "fs.outline", "code.outline", "code.symbols", "code.deps", "code.refs", "code.map", "git.status", "git.diff", "git.log", "git.show"];

/// The most characters of the answer of a helper that go back to the run that asked (the audit's 600 tokens, at about four characters to a token and a little less for code).
pub const HELPER_ANSWER_CHARS: usize = 1_500;

fn read_tools() -> Vec<String> {
    READ_TOOLS.iter().map(|t| t.to_string()).collect()
}

/// The built-in definitions, in the order they are listed.
pub fn all() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "build".into(),
            description: "The agent as it is: every tool the task calls for, the rules it always had. Asking for it is the same as asking for no agent.".into(),
            mode: Mode::Primary,
            prompt: String::new(),
            allowed_tools: Vec::new(),
            packs: Vec::new(),
            max_rounds: 0,
            max_seconds: 0,
            source: Source::Builtin,
        },
        AgentDef {
            name: "plan".into(),
            description: "Reads the project and writes a plan of at most five steps; it can read and search, and nothing else.".into(),
            mode: Mode::Primary,
            prompt: "You are the planning agent. Read what you need to understand the task, then answer with a plan of at most five numbered steps: what to change, in which file, and how to check it. You can only read and search. Do not change anything and do not run programs.".into(),
            allowed_tools: read_tools(),
            packs: vec!["files".into(), "git".into(), "code".into()],
            max_rounds: 12,
            max_seconds: 0,
            source: Source::Builtin,
        },
        AgentDef {
            name: "explore".into(),
            description: "Looks something up in the code in a context of its own and answers in a few lines: where a name is used, how a part works, which files a change would touch. It can only read and search.".into(),
            mode: Mode::Subagent,
            prompt: "You are a read-only explorer. Answer the question you are given, using only the tools listed: search, then read the lines that matter. You cannot change anything and cannot ask anybody. Answer in at most ten short lines: say where (file:line) and what, and say plainly what you could not find. Do not paste long code.".into(),
            allowed_tools: read_tools(),
            packs: vec!["files".into(), "git".into(), "code".into()],
            max_rounds: 6,
            max_seconds: 120,
            source: Source::Builtin,
        },
        AgentDef {
            name: "review".into(),
            description: "Reads a change (git diff, git log, the files it touches) and says what is risky about it; it can only read and search.".into(),
            mode: Mode::All,
            prompt: "You are a code reviewer. Read the change you are asked about with git.diff, git.log and fs.read, then list at most eight findings, the worst first, each as file:line, what is wrong and why. Say 'no findings' if there are none. You cannot change anything: do not suggest running commands you were not asked about.".into(),
            allowed_tools: read_tools(),
            packs: vec!["files".into(), "git".into(), "code".into()],
            max_rounds: 8,
            max_seconds: 180,
            source: Source::Builtin,
        },
    ]
}

/// Is this name one of the built-in ones (reserved)?
pub fn is_reserved(name: &str) -> bool {
    matches!(name, "build" | "plan" | "explore" | "review")
}
