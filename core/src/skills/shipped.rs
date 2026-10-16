//! The skills that ship with Aqua (card OA12-01; tasks WF-15 and WF-18 of the OpenCode audit, WF-16 for a diff that is on this computer).
//!
//! Ten small development skills, kept as text files in `core/assets/skills/<name>/SKILL.md` and compiled into the program with `include_str!`, in the same format as the skills a person
//! writes (`skills.rs`): `fix-failing-tests`, `debug-build`, `review-diff`, `write-tests`, `add-endpoint`, `db-migration`, `upgrade-deps`, `release-notes`, `incident-triage` and
//! `init-project`. Each is a few numbered steps for a small model, one family of tools, and ends in a check or a report.
//!
//! * **Off.** Until the person sets `AQUA_SHIPPED_SKILLS=on` they are not in the prompt, `skill.load` does not know them and they are not offered as commands. (They are named by `aqua-cli doctor`,
//!   with how to turn them on.) A skill list in the prompt costs tokens every round, so the new behaviour is the person's to switch on.
//! * **Theirs win.** A skill of the person's own (`data/skills`) or of a repository whose `.aqua` text they trusted that has the same name hides the shipped one.
//! * **Text that is Aqua's own.** The text is in the program, not on the disk: no tool can write it. It is trusted for that reason, and for no other: what a skill makes the agent read (a diff, a
//!   file, a log, command output) is text from outside, and the skill says so.
//! * **They can only narrow.** The `allowed-tools` of each skill is a list of names (no pattern) of tools that exist. A skill with such a list makes the run refuse every other tool, it
//!   never gives one (`ToolHost::restrict_run`). The lists hold nothing that reaches the network or another computer, nothing that cannot be undone, and no installer: a skill that
//!   would change a dependency says what to run and leaves the running of it to the person; any command a skill leads to is a `term.run` or `check.run` call, which goes through
//!   `ToolHost::call` and asks as it always did. The test in `core/tests/builtin_skills_test.rs` holds each list to a ceiling.
//! * **Not measured.** How well a small model follows these steps has not been measured (the benchmark needs the model, and no model was run for this card). They are short step lists
//!   written from the workflows of the audit (`review-diff` takes the rules of OpenCode's review prompt that the audit quotes, shortened), so the first measured result is still to come.
//! * **Not here.** Reading a pull request from a forge (it needs the network; the card for it is on hold), `deps.outdated` and `deps.audit` (they ask a registry), and more toolchains
//!   (the Go, .NET and Java allowlist is the owner's decision D-22): `review-diff` reads a diff that is on this computer, and `upgrade-deps` edits one version and runs the check.
//!
//! A shipped skill is a [`SkillSource::Shipped`] skill in the set a run reads ([`super::for_run`]).

use super::{parse_skill_full, Skill, SkillMeta, SkillSource, Skills};
use std::sync::{Arc, OnceLock};

/// Each shipped skill: the name of its folder and the text of its `SKILL.md`, in the order they are listed.
pub const FILES: &[(&str, &str)] = &[
    ("fix-failing-tests", include_str!("../../assets/skills/fix-failing-tests/SKILL.md")),
    ("debug-build", include_str!("../../assets/skills/debug-build/SKILL.md")),
    ("review-diff", include_str!("../../assets/skills/review-diff/SKILL.md")),
    ("write-tests", include_str!("../../assets/skills/write-tests/SKILL.md")),
    ("add-endpoint", include_str!("../../assets/skills/add-endpoint/SKILL.md")),
    ("db-migration", include_str!("../../assets/skills/db-migration/SKILL.md")),
    ("upgrade-deps", include_str!("../../assets/skills/upgrade-deps/SKILL.md")),
    ("release-notes", include_str!("../../assets/skills/release-notes/SKILL.md")),
    ("incident-triage", include_str!("../../assets/skills/incident-triage/SKILL.md")),
    ("init-project", include_str!("../../assets/skills/init-project/SKILL.md")),
];

/// Are the shipped skills on offer? Off until `AQUA_SHIPPED_SKILLS=on`.
pub fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::SHIPPED_SKILLS)
}

/// The names of the shipped skills, in the order they are listed.
pub fn names() -> Vec<&'static str> {
    FILES.iter().map(|(n, _)| *n).collect()
}

/// The shipped skills read from `files` (folder, text): a text that cannot be read is left out and named in `problems`.
pub fn read(files: &[(&str, &str)]) -> Skills {
    let mut parts: Vec<(Skill, SkillMeta)> = Vec::new();
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    for (folder, text) in files {
        match parse_skill_full(text, folder) {
            Ok(mut p) => {
                p.meta.source = SkillSource::Shipped;
                warnings.extend(p.warnings.iter().map(|w| format!("shipped skill {folder}: {w}")));
                parts.push((p.skill, p.meta));
            }
            Err(why) => problems.push(format!("shipped skill {folder}: {why}")),
        }
    }
    Skills::from_parts(parts, problems, warnings)
}

/// Every shipped skill, whether or not they are on offer (read once). What is on offer to a run is [`super::for_run`].
pub fn all() -> Arc<Skills> {
    static ALL: OnceLock<Arc<Skills>> = OnceLock::new();
    ALL.get_or_init(|| Arc::new(read(FILES))).clone()
}

/// The shipped skill of this name, **whether or not they are on offer**. It is for what only narrows: a run that goes on after a restart is held to the list of a skill it had loaded,
/// even when the switch has been turned off since.
pub fn lookup(name: &str) -> Option<Skill> {
    all().get(name).cloned()
}

/// One line for the doctor: how many ship, their names, and whether they are on.
pub fn status_line() -> String {
    let skills = all();
    let names = skills.names().join(", ");
    if enabled() {
        format!("{} skill(s) ship with Aqua and are on offer ({names})", skills.len())
    } else {
        format!("{} skill(s) ship with Aqua ({names}); they are off, so the agent is not offered them (set AQUA_SHIPPED_SKILLS=on to use them)", skills.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_text_is_read_and_none_is_left_out() {
        let all = all();
        assert!(all.problems.is_empty(), "{:?}", all.problems);
        assert!(all.warnings.is_empty(), "a key the format does not know: {:?}", all.warnings);
        assert_eq!(all.names(), names());
        assert_eq!(all.len(), FILES.len());
        for n in names() {
            assert_eq!(all.meta_of(n).map(|m| m.source), Some(SkillSource::Shipped), "{n}");
        }
    }

    #[test]
    fn a_text_that_cannot_be_read_is_named_and_the_others_still_work() {
        let s = read(&[("good", "---\nname: good\ndescription: Does a good thing\n---\nDo it."), ("bad", "no front matter"), ("other", "---\nname: wrong\ndescription: d\n---\nt")]);
        assert_eq!(s.names(), vec!["good"]);
        assert_eq!(s.problems.len(), 2, "{:?}", s.problems);
        assert!(s.problems[0].starts_with("shipped skill bad:") && s.problems[1].contains("not the folder's name"), "{:?}", s.problems);
    }

    #[test]
    fn lookup_finds_a_shipped_skill_when_the_switch_is_off() {
        crate::flags::with_overrides(&[("AQUA_SHIPPED_SKILLS", None)], || {
            assert!(!enabled());
            let s = lookup("review-diff").expect("a shipped skill is found by name, switch or not");
            assert!(!s.allowed_tools.is_empty() && s.allowed_tools.iter().all(|t| !t.contains('*')));
            assert!(lookup("not-a-skill").is_none());
        });
    }

    #[test]
    fn the_doctor_line_says_whether_they_are_on() {
        crate::flags::with_overrides(&[("AQUA_SHIPPED_SKILLS", None)], || {
            let off = status_line();
            assert!(off.starts_with("10 skill(s) ship with Aqua") && off.contains("they are off") && off.contains("AQUA_SHIPPED_SKILLS=on"), "{off}");
        });
        crate::flags::with_overrides(&[("AQUA_SHIPPED_SKILLS", Some("on"))], || {
            let on = status_line();
            assert!(on.contains("are on offer") && on.contains("review-diff") && !on.contains("they are off"), "{on}");
        });
    }
}
