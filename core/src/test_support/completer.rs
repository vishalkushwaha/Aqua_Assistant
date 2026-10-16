//! `ScriptedCompleter`: the model of the flows (`flows::planner::Completer`), as a script.
//!
//! The flows' planner, the comment drafts, the triage diagnosis and the vote all ask a `Completer` for text. A test gives them this one:
//! it answers from a list, in order (or from a function of the prompt), and remembers every prompt and the token budget it came with.

use crate::flows::planner::Completer;
use std::collections::VecDeque;
use std::sync::Mutex;

type AnswerFn = Box<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

pub struct ScriptedCompleter {
    replies: Mutex<VecDeque<Result<String, String>>>,
    answer: Option<AnswerFn>,
    prompts: Mutex<Vec<String>>,
    budgets: Mutex<Vec<u32>>,
}

impl ScriptedCompleter {
    /// Answers with these, in order: `Ok(text)` is a reply, `Err(message)` a model that failed. When the script is used up every further
    /// call fails with a sentence that says so ("the script ran out of replies").
    pub fn new(replies: &[Result<&str, &str>]) -> Self {
        Self::with(replies.iter().map(|r| (*r).map(str::to_string).map_err(str::to_string)).collect(), None)
    }

    /// Answers with these texts, in order.
    pub fn answers(texts: &[&str]) -> Self {
        Self::with(texts.iter().map(|t| Ok(t.to_string())).collect(), None)
    }

    /// Answers every call with `f(prompt)`.
    pub fn from_fn(f: impl Fn(&str) -> Result<String, String> + Send + Sync + 'static) -> Self {
        Self::with(VecDeque::new(), Some(Box::new(f)))
    }

    fn with(replies: VecDeque<Result<String, String>>, answer: Option<AnswerFn>) -> Self {
        Self { replies: Mutex::new(replies), answer, prompts: Mutex::new(Vec::new()), budgets: Mutex::new(Vec::new()) }
    }

    /// Every prompt it was asked, in order.
    pub fn asked(&self) -> Vec<String> {
        self.prompts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The token budget of every call that came with one (`complete_with_budget`), in order.
    pub fn budgets(&self) -> Vec<u32> {
        self.budgets.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How many scripted replies are left.
    pub fn left(&self) -> usize {
        self.replies.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Completer for ScriptedCompleter {
    fn complete(&self, prompt: &str) -> Result<String, String> {
        self.prompts.lock().unwrap_or_else(|e| e.into_inner()).push(prompt.to_string());
        if let Some(answer) = &self.answer {
            return answer(prompt);
        }
        self.replies.lock().unwrap_or_else(|e| e.into_inner()).pop_front().unwrap_or_else(|| Err("the script ran out of replies".into()))
    }

    fn complete_with_budget(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        self.budgets.lock().unwrap_or_else(|e| e.into_inner()).push(max_tokens);
        self.complete(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_answers_in_order_remembers_the_prompts_and_then_runs_out() {
        let m = ScriptedCompleter::new(&[Ok("first"), Err("the language model is not ready"), Ok("third")]);
        assert_eq!(m.complete("a"), Ok("first".to_string()));
        assert_eq!(m.complete("b"), Err("the language model is not ready".to_string()));
        assert_eq!(m.left(), 1);
        assert_eq!(m.complete("c"), Ok("third".to_string()));
        assert!(m.complete("d").unwrap_err().contains("script ran out"));
        assert_eq!(m.asked(), ["a", "b", "c", "d"]);
    }

    #[test]
    fn the_budget_is_remembered_and_the_call_is_a_call() {
        let m = ScriptedCompleter::answers(&["x", "y"]);
        assert_eq!(m.complete_with_budget("p", 700).unwrap(), "x");
        assert_eq!(m.complete("q").unwrap(), "y");
        assert_eq!((m.budgets(), m.asked()), (vec![700], vec!["p".to_string(), "q".to_string()]));
    }

    #[test]
    fn a_function_answers_from_the_prompt() {
        let m = ScriptedCompleter::from_fn(|p| if p.contains("spam") { Err("no".into()) } else { Ok(format!("{} chars", p.len())) });
        assert_eq!(m.complete("hello").unwrap(), "5 chars");
        assert!(m.complete("spam spam").is_err());
        assert_eq!(m.asked().len(), 2);
    }
}
