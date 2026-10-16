//! The words: one plain sentence for each cause the rules know and for each of the twelve error classes, with up to three next steps.
//!
//! This is fixed text. No model writes any of it, and nothing from the failure goes in but a tool name, a path or an argument name that
//! the caller has already cleaned, and (for a class only) a short quote that has had its secrets masked.

use super::Cause;
use crate::flows::supervisor::classify::ErrorClass;

/// A sentence for the person and what they can do about it (at most three steps).
#[derive(Debug, Clone, PartialEq)]
pub struct Words {
    pub why: String,
    pub next_steps: Vec<String>,
}

fn words(why: String, steps: &[&str]) -> Words {
    Words { why, next_steps: steps.iter().take(3).map(|s| s.to_string()).collect() }
}

/// How a tool is named inside a sentence.
fn subject(tool: &str) -> String {
    if tool.trim().is_empty() {
        "that step".to_string()
    } else {
        format!("the tool {}", tool.trim())
    }
}

/// The sentence starts with a capital letter, whatever the subject was.
fn cap(s: String) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => s,
    }
}

/// The sentence for a cause the rules know. `detail` is what the cause names (a path, an argument, a tool), already cleaned, or empty.
/// `Cause::ByClass` has no sentence of its own: use [`for_class`].
pub fn for_cause(cause: Cause, tool: &str, detail: &str) -> Words {
    let s = subject(tool);
    match cause {
        Cause::OutsideFolders => {
            let what = if detail.is_empty() { "that".to_string() } else { detail.to_string() };
            words(format!("I could not use {what} because it is outside the folders I am allowed to use."), &["Ask me to allow that folder", "Or pick a folder I may already use"])
        }
        Cause::MissingArgument => {
            let what = if detail.is_empty() { "something it needs".to_string() } else { format!("the '{detail}' it needs") };
            words(cap(format!("{s} was called without {what}, so it did nothing.")), &["Say the missing detail, for example the file name or the address", "Then ask me to try again"])
        }
        Cause::ToolNotInstalled => words(format!("The program behind {s} is not installed on this computer, so I could not use it."), &["Install the program, or set where it is in the tool's settings", "Or ask me to do it another way"]),
        Cause::WrongToolName => {
            let what = if detail.is_empty() { "a tool".to_string() } else { format!("a tool called {detail}") };
            words(format!("I tried to use {what}, and there is no tool with that name."), &["Check the name of the tool", "Ask again in other words, so that I choose a tool that exists"])
        }
        Cause::PersonDeclined if tool.trim() == "fs.allow_folder" => words("You did not allow that folder, so I could not use it.".to_string(), &["Ask again if you change your mind", "Or tell me to use a folder I may already use"]),
        Cause::PersonDeclined => words(format!("You did not approve {s}, so I stopped without doing it."), &["Ask again if you want it done", "Or tell me to do it another way"]),
        Cause::PolicyRefused => words(format!("A rule in Aqua's settings does not allow {s} to do that, so I stopped."), &["Look at the rules for that tool in Settings if you want to allow it", "Or ask me for something that stays inside the rules"]),
        Cause::NeedsLogin => words("The site is asking you to sign in, and I do not sign in for you.".to_string(), &["Sign in yourself in the browser", "Then ask me again"]),
        Cause::VoiceCannotAct => words("A request spoken aloud may only look at the screen and open apps, so I did not do that.".to_string(), &["Type the request in the app instead"]),
        Cause::BrowserUnavailable => words("The browser you named is not installed, so I did not use another one.".to_string(), &["Install that browser", "Or tell me to use the browser that is installed"]),
        Cause::OutOfTime => words("I ran out of time before the task was done.".to_string(), &["Ask for one smaller part of it", "Try again when the computer is less busy"]),
        Cause::StepLimit => words("I used all my steps without finishing the task.".to_string(), &["Ask for a smaller task", "Name the exact file, folder or page to start with"]),
        Cause::GoingRoundInCircles => words("I stopped because I kept repeating the same step and was getting nowhere.".to_string(), &["Say it another way", "Give me the exact name or place"]),
        Cause::ModelReply => words("The language model did not give an answer I could use, so I stopped.".to_string(), &["Try again in a moment", "If it keeps happening, restart Aqua"]),
        Cause::Stopped => words("The run was stopped before it finished.".to_string(), &["Ask again whenever you like"]),
        Cause::Interrupted => words("Aqua was closed or stopped while this was running.".to_string(), &["Ask me to go on from where it stopped", "Or ask for it again"]),
        Cause::QuestionExpired => words("I asked you a question and it was not answered in time.".to_string(), &["Ask again when you can answer"]),
        Cause::CapReached => {
            let limit = match detail {
                "max_seconds" => "how long a run may take",
                "max_tool_calls" => "how many tool calls a run may make",
                "max_files_changed" => "how many files a run may change",
                "max_changed_bytes" => "how much a run may write",
                _ => "what a run may do",
            };
            words(format!("The limit you set for {limit} was reached, so the run was stopped."), &["Raise that limit in the autonomy setting if you want it to go further", "Or ask for a smaller task"])
        }
        Cause::ByClass => for_class(ErrorClass::Unknown, tool, detail),
    }
}

/// The sentence for a class when the rules found no particular cause. `excerpt` is a short quote of what the failure said (secrets already
/// masked), or empty.
pub fn for_class(class: ErrorClass, tool: &str, excerpt: &str) -> Words {
    let s = subject(tool);
    let quote = if excerpt.is_empty() { String::new() } else { format!(" It said: \"{excerpt}\"") };
    let mut w = match class {
        ErrorClass::Authentication => words(format!("The account or key was not accepted, so {s} could not go on."), &["Sign in again, or check the key in Settings", "Then ask me to try again"]),
        ErrorClass::Network => words(format!("The connection failed, so {s} could not reach what it needs."), &["Check your internet connection", "Try again in a minute"]),
        ErrorClass::Timeout => words(cap(format!("{s} took too long to answer, so I stopped waiting.")), &["Try again later", "Ask for something smaller"]),
        ErrorClass::RateLimit => words(format!("The service asked me to slow down, so {s} stopped."), &["Wait a few minutes", "Then try again"]),
        ErrorClass::InvalidInput => words(format!("A value given to {s} was not valid."), &["Say what you meant again with more detail, such as a name or a path", "Check the spelling of the names"]),
        ErrorClass::MissingFile => words(cap(format!("A file or folder that {s} needs is not there.")), &["Check the name and the folder", "Tell me the exact path"]),
        ErrorClass::BrowserError => words(format!("The browser had a problem while {s} was using it."), &["Close the browser window Aqua opened", "Then ask me to try again"]),
        ErrorClass::WebsiteUiChange => words(format!("The page no longer looks the way {s} expected."), &["Open the page yourself to see what changed", "Ask me to try a different way"]),
        ErrorClass::ApiError => words(format!("The service answered {s} with an error."), &["Try again later", "Check that the address and the details are right"]),
        ErrorClass::PermissionError => words(format!("I am not allowed to do that with {s}."), &["Look at what is allowed in Settings", "Or ask me for something else"]),
        ErrorClass::ExternalService => words(format!("The outside service is having trouble, so {s} could not finish."), &["Try again later", "Check the status page of the service"]),
        ErrorClass::Unknown => words("It stopped, and the message does not say why.".to_string(), &["Look at the steps of this run to see the last thing that happened", "Ask me to try again, or say it another way"]),
    };
    w.why.push_str(&quote);
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASSES: [ErrorClass; 12] = [
        ErrorClass::Authentication,
        ErrorClass::Network,
        ErrorClass::Timeout,
        ErrorClass::RateLimit,
        ErrorClass::InvalidInput,
        ErrorClass::MissingFile,
        ErrorClass::BrowserError,
        ErrorClass::WebsiteUiChange,
        ErrorClass::ApiError,
        ErrorClass::PermissionError,
        ErrorClass::ExternalService,
        ErrorClass::Unknown,
    ];

    #[test]
    fn every_class_has_one_sentence_and_one_to_three_next_steps() {
        for class in CLASSES {
            let w = for_class(class, "fs.read", "");
            assert!(w.why.ends_with('.') && w.why.matches(". ").count() == 0, "{class:?}: one sentence: {}", w.why);
            assert!((1..=3).contains(&w.next_steps.len()), "{class:?}: {:?}", w.next_steps);
            assert!(w.why.chars().next().unwrap().is_uppercase(), "{class:?}: {}", w.why);
        }
    }

    #[test]
    fn every_cause_has_a_sentence_and_the_steps_are_words_a_person_can_follow() {
        let causes = [
            Cause::OutsideFolders,
            Cause::MissingArgument,
            Cause::ToolNotInstalled,
            Cause::WrongToolName,
            Cause::PersonDeclined,
            Cause::PolicyRefused,
            Cause::NeedsLogin,
            Cause::VoiceCannotAct,
            Cause::BrowserUnavailable,
            Cause::OutOfTime,
            Cause::StepLimit,
            Cause::GoingRoundInCircles,
            Cause::ModelReply,
            Cause::Stopped,
            Cause::Interrupted,
            Cause::QuestionExpired,
            Cause::CapReached,
        ];
        for cause in causes {
            let w = for_cause(cause, "fs.write", "x");
            assert!(w.why.ends_with('.') && w.why.chars().next().unwrap().is_uppercase(), "{cause:?}: {}", w.why);
            assert!((1..=3).contains(&w.next_steps.len()), "{cause:?}: {:?}", w.next_steps);
            assert!(w.next_steps.iter().all(|s| !s.is_empty() && !s.contains("ERROR") && !s.contains('[')), "{cause:?}: {:?}", w.next_steps);
        }
    }

    #[test]
    fn a_sentence_that_starts_with_the_tool_still_starts_with_a_capital_letter() {
        assert_eq!(for_class(ErrorClass::Timeout, "web.read", "").why, "The tool web.read took too long to answer, so I stopped waiting.");
        assert_eq!(for_class(ErrorClass::Timeout, "", "").why, "That step took too long to answer, so I stopped waiting.");
        assert_eq!(for_cause(Cause::MissingArgument, "fs.read", "path").why, "The tool fs.read was called without the 'path' it needs, so it did nothing.");
    }

    #[test]
    fn the_quote_of_what_the_failure_said_is_added_for_a_class_and_not_for_a_cause() {
        assert!(for_class(ErrorClass::Unknown, "", "the disk went away").why.ends_with("It said: \"the disk went away\""));
        assert!(!for_class(ErrorClass::Unknown, "", "").why.contains("It said"));
        assert!(!for_cause(Cause::PolicyRefused, "term.run", "a secret").why.contains("a secret"), "a cause names a path or an argument only where its sentence has a place for one");
    }
}
