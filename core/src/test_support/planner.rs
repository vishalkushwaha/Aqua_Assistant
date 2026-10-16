//! `ScriptedPlanner`: the model of the agent loop, as a script.
//!
//! It answers the loop's questions with the replies it was given, in order, and remembers what it was asked. Nothing is shared between
//! two of them, so a test makes its own. It replaced the copy in `modules/browser_agent.rs` (and is the one place to extend when a test
//! needs another kind of reply).

use crate::modules::planner::{PlanReply, PlanRequest, Planner};
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

/// One answer of the script.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The text of the reply (the loop reads it as the model's JSON).
    Text(String),
    /// The call fails with this message (a model server that would not answer).
    Error(String),
    /// The call stopped so that a person could have the model (card R6-03, T6-03.4): an ordinary failure while yielding is off, the
    /// server's own "yielded" message otherwise.
    Yield,
    /// Waits this long first, then gives the inner reply (a slow model). The wait is in short steps so that the loop's clock ticks.
    After(Duration, Box<Reply>),
    /// The model server refused the request with this message (an HTTP 400): a `RequestRefused`, the type the engine gives it. A message that says the prompt did not fit makes it a
    /// context overflow; see [`Reply::too_long`] (card OA4-02).
    Refused(String),
    /// The model was working for another request and this one was not served: the sentence of the server layer, with no type, so the loop must find it by the words.
    Busy,
    /// The model server could not be used: an error the planner has classed as `Unavailable`, as `LlamaPlanner` does after the server failed.
    Unavailable(String),
    /// The text of a reply, and what the model server measured for it (`prompt_n` read and `cache_n` taken from its cache): what teaches the token meter.
    Timed(String, crate::modules::llm_router::llama_server::Timings),
}

impl Reply {
    /// The refusal of a prompt that did not fit a window of `window` tokens and had `prompt_tokens`, in the words the server layer writes (`llama_server::refusal_message`).
    pub fn too_long(window: u64, prompt_tokens: u64) -> Reply {
        Reply::Refused(format!("the language model refused this request (HTTP 400: exceeds the available context size ({window} tokens; the prompt has {prompt_tokens}))"))
    }

    /// The same refusal as an older server writes it, with no numbers.
    pub fn too_long_without_numbers(window: u64) -> Reply {
        Reply::Refused(format!("the language model refused this request (HTTP 400: exceeds the available context size ({window} tokens))"))
    }
}

impl From<&str> for Reply {
    fn from(text: &str) -> Self {
        Reply::Text(text.to_string())
    }
}

impl From<String> for Reply {
    fn from(text: String) -> Self {
        Reply::Text(text)
    }
}

/// What the planner was asked at one round.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub prompt: String,
    /// Whether the loop asked for a reply held to a JSON shape (`PlanRequest::constraint`).
    pub constrained: bool,
}

/// A planner that says what it is told to, and remembers what it was asked.
pub struct ScriptedPlanner {
    replies: Mutex<VecDeque<Reply>>,
    /// How many replies the script had to begin with.
    scripted: usize,
    seen: Mutex<Vec<Seen>>,
    /// The schema each request was held to (`None`: it was not held to one), in the order of the requests.
    schemas: Mutex<Vec<Option<serde_json::Value>>>,
    /// The window the planner says it has, in tokens.
    window: u32,
    /// What the loop asked it to warm.
    warmed: Mutex<Vec<String>>,
    /// Whether warming fails (a server that would not take the request).
    warm_fails: bool,
    /// Plays a planner whose model layer asks again by itself for a prompt the server refuses as too long (`LlamaPlanner` with `AQUA_MODEL_ROUTER=on`, card OA9-02): see [`Self::router_asks_again`].
    router_asks_again: bool,
}

impl ScriptedPlanner {
    pub fn new(replies: Vec<Reply>) -> Self {
        Self { scripted: replies.len(), replies: Mutex::new(replies.into()), seen: Mutex::new(Vec::new()), schemas: Mutex::new(Vec::new()), window: 4096, warmed: Mutex::new(Vec::new()), warm_fails: false, router_asks_again: false }
    }

    /// A script of plain texts. The text `<yield>` is [`Reply::Yield`] (the spelling the loop's tests have always used).
    pub fn texts(replies: &[&str]) -> Self {
        Self::new(replies.iter().map(|r| if *r == "<yield>" { Reply::Yield } else { Reply::Text((*r).to_string()) }).collect())
    }

    /// The window the planner says it has (default 4096 tokens).
    pub fn with_window(mut self, tokens: u32) -> Self {
        self.window = tokens;
        self
    }

    /// Warming the start of the prompt fails, as a server that would not take the request does.
    pub fn warm_fails(mut self) -> Self {
        self.warm_fails = true;
        self
    }

    /// Plays the model layer of `LlamaPlanner` with the router on: when the script refuses a prompt as too long and the loop gave a way to shorten it, the planner asks the loop for the
    /// shorter prompt and makes the second request itself, in the same call (the refusal and the second request are both in [`Self::asked`]). Without this the planner ignores that way,
    /// as every planner but the local one does.
    pub fn router_asks_again(mut self) -> Self {
        self.router_asks_again = true;
        self
    }

    /// Every request so far, in order.
    pub fn asked(&self) -> Vec<Seen> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The JSON schema each request so far was held to, in order (`None` for a request with no constraint or with a grammar).
    pub fn schemas(&self) -> Vec<Option<serde_json::Value>> {
        self.schemas.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The prompt of every request so far.
    pub fn prompts(&self) -> Vec<String> {
        self.asked().into_iter().map(|s| s.prompt).collect()
    }

    /// Every start of a prompt the loop asked it to warm.
    pub fn warmed(&self) -> Vec<String> {
        self.warmed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How many replies are left.
    pub fn left(&self) -> usize {
        self.replies.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn deliver(reply: Reply, on_tick: &mut dyn FnMut(u64)) -> Result<(String, Option<crate::modules::llm_router::llama_server::Timings>)> {
        match reply {
            Reply::Text(text) => Ok((text, None)),
            Reply::Timed(text, timings) => Ok((text, Some(timings))),
            Reply::Error(message) => anyhow::bail!(message),
            Reply::Refused(message) => Err(anyhow::Error::new(crate::modules::llm_router::llama_engine::RequestRefused(message))),
            Reply::Busy => anyhow::bail!(crate::modules::llm_router::llama_server::busy_message(120)),
            Reply::Unavailable(message) => Err(crate::modules::planner::PlanError::new(crate::modules::planner::PlanErrorKind::Unavailable, message)),
            Reply::Yield => {
                // The model server never ends a call that was made without yielding in a yield: it is an ordinary failure then, and the loop does not ask again.
                if crate::modules::llm_router::llama_server::yielding_is_off() {
                    anyhow::bail!("the model server did not answer");
                }
                anyhow::bail!(crate::modules::llm_router::llama_server::yielded_message());
            }
            Reply::After(delay, inner) => {
                let started = std::time::Instant::now();
                while started.elapsed() < delay {
                    // A pause of the script (a slow model), not a wait for a signal.
                    std::thread::sleep((delay - started.elapsed()).min(Duration::from_millis(50)));
                    on_tick(started.elapsed().as_secs());
                }
                Self::deliver(*inner, on_tick)
            }
        }
    }
}

impl Planner for ScriptedPlanner {
    fn model_name(&self) -> String {
        "scripted".into()
    }

    fn max_context_tokens(&self) -> u32 {
        self.window
    }

    fn warm_prefix(&self, prefix: &str) -> Result<()> {
        self.warmed.lock().unwrap_or_else(|e| e.into_inner()).push(prefix.to_string());
        if self.warm_fails {
            anyhow::bail!("the server did not take the request");
        }
        Ok(())
    }

    fn plan(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64)) -> Result<PlanReply> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).push(Seen { prompt: req.prompt.to_string(), constrained: req.constraint.is_some() });
        self.schemas.lock().unwrap_or_else(|e| e.into_inner()).push(match req.constraint {
            Some(crate::modules::llm_router::llama_server::Constraint::JsonSchema(schema)) => Some(schema.clone()),
            _ => None,
        });
        let reply = self.replies.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        let Some(reply) = reply else {
            let asked = self.seen.lock().unwrap_or_else(|e| e.into_inner()).len();
            anyhow::bail!("the planner was asked more often than the test expected ({asked} requests, {} replies were scripted)", self.scripted);
        };
        let (text, timings) = Self::deliver(reply, on_tick)?;
        Ok(PlanReply { text, timings })
    }

    fn plan_shrinking(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64), shrink: crate::modules::planner::PromptShrink<'_>) -> Result<PlanReply> {
        let first = self.plan(req, on_tick);
        if !self.router_asks_again {
            return first;
        }
        match first {
            Err(e) if crate::modules::planner::PlanErrorKind::of(&e) == crate::modules::planner::PlanErrorKind::ContextOverflow => match shrink(&crate::model::ProviderError::from_anyhow(&e)) {
                Some(shorter) => self.plan(&PlanRequest { prompt: &shorter, max_tokens: req.max_tokens, constraint: req.constraint }, on_tick),
                None => Err(e),
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(p: &ScriptedPlanner, prompt: &str) -> Result<String> {
        p.plan(&PlanRequest { prompt, max_tokens: 100, constraint: None }, &mut |_| {}).map(|r| r.text)
    }

    #[test]
    fn it_answers_in_order_and_remembers_every_prompt() {
        let p = ScriptedPlanner::texts(&["one", "two"]);
        assert_eq!(ask(&p, "first").unwrap(), "one");
        assert_eq!(ask(&p, "second").unwrap(), "two");
        assert_eq!(p.prompts(), ["first", "second"]);
        assert_eq!(p.asked()[1], Seen { prompt: "second".into(), constrained: false });
        assert_eq!(p.left(), 0);
    }

    #[test]
    fn an_empty_script_fails_with_a_sentence_that_counts() {
        let p = ScriptedPlanner::texts(&["only"]);
        ask(&p, "a").unwrap();
        let e = ask(&p, "b").unwrap_err().to_string();
        assert!(e.starts_with("the planner was asked more often than the test expected"), "{e}");
        assert!(e.contains("2 requests") && e.contains("1 replies were scripted"), "{e}");
        let e = ask(&p, "c").unwrap_err().to_string();
        assert!(e.contains("3 requests") && e.contains("1 replies were scripted"), "{e}");
        assert_eq!(p.asked().len(), 3, "a request that found the script empty is remembered too");
    }

    #[test]
    fn an_error_reply_fails_the_call_and_the_script_goes_on() {
        let p = ScriptedPlanner::new(vec![Reply::Error("the model server did not answer".into()), "next".into()]);
        assert_eq!(ask(&p, "a").unwrap_err().to_string(), "the model server did not answer");
        assert_eq!(ask(&p, "b").unwrap(), "next");
    }

    #[test]
    fn the_constraint_is_remembered_as_a_flag() {
        let p = ScriptedPlanner::texts(&["x"]);
        let constraint = crate::modules::llm_router::llama_server::Constraint::JsonSchema(serde_json::json!({"type": "object"}));
        p.plan(&PlanRequest { prompt: "p", max_tokens: 10, constraint: Some(&constraint) }, &mut |_| {}).unwrap();
        assert!(p.asked()[0].constrained);
    }

    #[test]
    fn the_window_and_warming_are_the_ones_the_test_chose() {
        let roomy = ScriptedPlanner::texts(&[]);
        assert_eq!(roomy.max_context_tokens(), 4096);
        assert_eq!(roomy.model_name(), "scripted");
        let narrow = ScriptedPlanner::texts(&[]).with_window(512);
        assert_eq!(narrow.max_context_tokens(), 512);
        roomy.warm_prefix("the start").unwrap();
        assert_eq!(roomy.warmed(), ["the start"]);
        let failing = ScriptedPlanner::texts(&[]).warm_fails();
        assert!(failing.warm_prefix("x").is_err());
        assert_eq!(failing.warmed(), ["x"], "it was asked to warm, and failed");
    }

    #[test]
    fn a_slow_reply_waits_then_answers_and_ticks_while_it_waits() {
        let p = ScriptedPlanner::new(vec![Reply::After(Duration::from_millis(120), Box::new("late".into()))]);
        let mut ticks = 0;
        let started = std::time::Instant::now();
        let r = p.plan(&PlanRequest { prompt: "p", max_tokens: 10, constraint: None }, &mut |_| ticks += 1).unwrap();
        assert_eq!(r.text, "late");
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert!(ticks >= 2, "the clock was told while it waited: {ticks}");
    }

    #[test]
    fn the_yield_text_is_a_yield() {
        let p = ScriptedPlanner::texts(&["<yield>"]);
        // Whichever way yielding is switched in this process, the call fails and says why in words.
        let e = ask(&p, "a").unwrap_err().to_string();
        assert!(!e.is_empty(), "a yield is a failed call");
        assert!(matches!(ScriptedPlanner::new(vec![Reply::Yield]).replies.lock().unwrap()[0], Reply::Yield));
    }
}
