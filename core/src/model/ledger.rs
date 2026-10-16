//! What each call to the model cost, as the model server measured it (card OA9-05, task PR-10 of the OpenCode audit).
//!
//! The numbers are the server's own, not an estimate: llama.cpp says in each reply how many prompt tokens it read (`prompt_n`), how many it took from its cache (`cache_n`) and how many it
//! wrote (`predicted_n`), and how long each took. The run record has kept them since the event log (card OA10-01): a planner reply's step row holds `tokens_in` (what the model was
//! shown: read plus cached), `cache_read`, `tokens_out`, `ms`, `ttft_ms` and `decode_tps_x10`, and the run's `usage` event says the same to a client that follows the run. **This is not a
//! second place that records them.** It reads the rows that are there and sums them (per call, per run, per model), in the same words, so that `GET /api/v1/runs/{id}/usage` and the
//! record cannot disagree.
//!
//! * A call holds numbers and names, never text: no prompt, no reply, no tool argument. A reply that the server gave no numbers for is counted in `unmeasured_replies` and is in no
//!   total, so a total is a floor and says so.
//! * `provider` is `llama.cpp`: the planner's calls go to the local server, and the run record does not keep the provider of a call (it keeps the model). A provider that bills (a model on
//!   another machine, card OA9-04) has a price, `cost_usd`, that the local server does not.
//! * What this does **not** do (deferred in the card's report): a call that is not a step of a run (a chat turn, a decision, a warm-up) is not here, the count of tokens of a prompt before
//!   it is sent is still the meter's (`modules::context::meter`, which learns characters per token from these same numbers), and nothing asks the server's `/tokenize`.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The provider of a planner reply: the local llama.cpp server.
pub const LOCAL_PROVIDER: &str = "llama.cpp";

/// What one call cost.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CallUsage {
    /// The step of the run's loop the reply belongs to.
    pub step: u32,
    /// What the call was for: `agent` for the planner's replies.
    pub role: String,
    pub provider: String,
    pub model: String,
    /// Prompt tokens the model was shown for this reply: what the server read and what it took from its cache.
    pub tokens_in: u64,
    /// Of `tokens_in`, the tokens the server took from its cache (it did not read them again).
    pub cache_read: u64,
    /// Tokens the model wrote.
    pub tokens_out: u64,
    /// How long the round took, in milliseconds (the planner's whole call, queue and read and write).
    pub ms: u64,
    /// The wait before the first token (the server reading the part of the prompt it did not have).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_tokens_per_second: Option<f64>,
    /// What the call was billed, when the provider bills (never the local server).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Calls added up.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageTotals {
    pub calls: u64,
    pub tokens_in: u64,
    pub cache_read: u64,
    pub tokens_out: u64,
    pub ms: u64,
    /// Of the prompt tokens, the share the server did not have to read (0 to 1, four decimals); absent while no prompt token was counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_share: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl UsageTotals {
    pub fn add(&mut self, call: &CallUsage) {
        self.calls += 1;
        self.tokens_in += call.tokens_in;
        self.cache_read += call.cache_read;
        self.tokens_out += call.tokens_out;
        self.ms += call.ms;
        if let Some(cost) = call.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
        self.cache_hit_share = (self.tokens_in > 0).then(|| (self.cache_read as f64 / self.tokens_in as f64 * 10_000.0).round() / 10_000.0);
    }

    /// The calls added up.
    pub fn of(calls: &[CallUsage]) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for call in calls {
            totals.add(call);
        }
        totals
    }
}

/// What a run cost: each planner reply, the total, and the total by model.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunUsage {
    pub run_id: String,
    pub calls: Vec<CallUsage>,
    pub totals: UsageTotals,
    pub by_model: BTreeMap<String, UsageTotals>,
    /// Planner replies of the run for which the server gave no numbers: they are in no total above, which is then a floor.
    pub unmeasured_replies: u64,
}

fn count(value: &Value) -> Option<u64> {
    value.as_i64().map(|n| n.max(0) as u64)
}

impl RunUsage {
    /// The usage of a run, from the run as `AgentRunStore::get` gives it: its `id`, its `model` and its `steps`. Only a planner reply (a step of kind `plan`) is a call; its numbers are
    /// the ones the loop wrote when the reply came. The text of a reply (`planner_reply`) is not read.
    pub fn from_run(run: &Value) -> RunUsage {
        let run_id = run["id"].as_str().unwrap_or("").to_string();
        let model = run["model"].as_str().filter(|m| !m.is_empty()).unwrap_or("unknown").to_string();
        let mut calls = Vec::new();
        let mut unmeasured_replies = 0u64;
        for row in run["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if row["kind"] != "plan" {
                continue;
            }
            let (tokens_in, tokens_out) = (count(&row["tokens_in"]), count(&row["tokens_out"]));
            if tokens_in.is_none() && tokens_out.is_none() {
                unmeasured_replies += 1;
                continue;
            }
            calls.push(CallUsage {
                step: row["step"].as_u64().unwrap_or(0) as u32,
                role: "agent".to_string(),
                provider: LOCAL_PROVIDER.to_string(),
                model: model.clone(),
                tokens_in: tokens_in.unwrap_or(0),
                // A cache that read more than the prompt held is a number that cannot be right; it is cut to the prompt.
                cache_read: count(&row["cache_read"]).unwrap_or(0).min(tokens_in.unwrap_or(0)),
                tokens_out: tokens_out.unwrap_or(0),
                ms: count(&row["ms"]).unwrap_or(0),
                ttft_ms: count(&row["ttft_ms"]),
                decode_tokens_per_second: row["decode_tokens_per_second"].as_f64(),
                cost_usd: None,
            });
        }
        let mut by_model: BTreeMap<String, UsageTotals> = BTreeMap::new();
        for call in &calls {
            by_model.entry(call.model.clone()).or_default().add(call);
        }
        RunUsage { run_id, totals: UsageTotals::of(&calls), calls, by_model, unmeasured_replies }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(step: u64, tokens_in: Value, cache_read: Value, tokens_out: Value, ms: u64) -> Value {
        json!({"seq": step, "step": step, "kind": "plan", "tool": "", "ms": ms, "tokens_in": tokens_in, "tokens_out": tokens_out, "cache_read": cache_read, "ttft_ms": 310, "decode_tokens_per_second": 8.5, "planner_reply": "{\"tool\":\"fs.read\"}"})
    }

    #[test]
    fn a_runs_planner_replies_are_summed_by_run_and_by_model_from_the_numbers_the_server_gave() {
        let run = json!({"id": "run-1", "model": "qwen3-8b", "steps": [
            plan(1, json!(1400), json!(0), json!(40), 21_000),
            {"seq": 2, "step": 1, "kind": "tool", "tool": "fs.read", "tokens_in": null, "tokens_out": null, "cache_read": null, "ms": 12},
            plan(2, json!(1500), json!(1300), json!(25), 9_000),
            plan(3, json!(1600), json!(1500), json!(30), 8_000),
        ]});
        let usage = RunUsage::from_run(&run);
        assert_eq!(usage.run_id, "run-1");
        assert_eq!(usage.calls.len(), 3, "a tool call is not a call to the model");
        assert_eq!(usage.calls[1], CallUsage {
            step: 2,
            role: "agent".into(),
            provider: "llama.cpp".into(),
            model: "qwen3-8b".into(),
            tokens_in: 1500,
            cache_read: 1300,
            tokens_out: 25,
            ms: 9_000,
            ttft_ms: Some(310),
            decode_tokens_per_second: Some(8.5),
            cost_usd: None,
        });
        assert_eq!((usage.totals.calls, usage.totals.tokens_in, usage.totals.cache_read, usage.totals.tokens_out, usage.totals.ms), (3, 4500, 2800, 95, 38_000));
        // 2,800 of 4,500 prompt tokens did not have to be read again.
        assert_eq!(usage.totals.cache_hit_share, Some(0.6222));
        assert_eq!(usage.by_model.len(), 1);
        assert_eq!(usage.by_model["qwen3-8b"], usage.totals);
        assert_eq!(usage.unmeasured_replies, 0);
    }

    #[test]
    fn a_reply_the_server_gave_no_numbers_for_is_counted_as_unmeasured_and_is_in_no_total() {
        let run = json!({"id": "run-2", "model": "", "steps": [
            plan(1, Value::Null, Value::Null, Value::Null, 5_000),
            plan(2, json!(900), Value::Null, json!(20), 6_000),
        ]});
        let usage = RunUsage::from_run(&run);
        assert_eq!(usage.unmeasured_replies, 1);
        assert_eq!(usage.calls.len(), 1);
        assert_eq!(usage.calls[0].model, "unknown", "a run that never learned its model's name");
        assert_eq!((usage.calls[0].tokens_in, usage.calls[0].cache_read), (900, 0), "no cache number is a cache that gave nothing");
        assert_eq!(usage.totals.tokens_in, 900);
        // A run with no step at all, or a record that is not one, is an empty ledger and not an error.
        for run in [json!({"id": "run-3", "model": "m", "steps": []}), json!({}), json!({"id": "run-4", "steps": null})] {
            let empty = RunUsage::from_run(&run);
            assert_eq!((empty.calls.len(), empty.totals.calls, empty.totals.cache_hit_share, empty.unmeasured_replies), (0, 0, None, 0));
        }
    }

    #[test]
    fn a_number_that_cannot_be_right_is_cut_and_the_ledger_holds_no_text() {
        let run = json!({"id": "run-5", "model": "m", "steps": [plan(1, json!(100), json!(500), json!(-3), 10)]});
        let usage = RunUsage::from_run(&run);
        assert_eq!((usage.calls[0].tokens_in, usage.calls[0].cache_read, usage.calls[0].tokens_out), (100, 100, 0), "the cache cannot have given more than the prompt held, and a count is never negative");
        // Nothing of what the model said or was asked is in what the ledger says.
        let text = serde_json::to_string(&usage).unwrap();
        assert!(!text.contains("fs.read") && !text.contains("planner_reply"), "{text}");
        assert!(text.contains("\"tokens_in\":100") && text.contains("\"cache_hit_share\":1.0"), "{text}");
    }

    #[test]
    fn totals_add_up_calls_one_at_a_time_and_a_price_is_summed_only_when_there_is_one() {
        let call = |tokens_in, cost| CallUsage { step: 1, role: "agent".into(), provider: "x".into(), model: "m".into(), tokens_in, cache_read: 0, tokens_out: 1, ms: 1, ttft_ms: None, decode_tokens_per_second: None, cost_usd: cost };
        let mut totals = UsageTotals::default();
        assert_eq!(totals.cost_usd, None);
        totals.add(&call(10, None));
        assert_eq!(totals.cost_usd, None, "a local call has no price");
        totals.add(&call(10, Some(0.002)));
        totals.add(&call(10, Some(0.001)));
        assert_eq!((totals.calls, totals.tokens_in), (3, 30));
        assert!((totals.cost_usd.unwrap() - 0.003).abs() < 1e-12);
        assert_eq!(UsageTotals::of(&[call(5, None), call(7, None)]).tokens_in, 12);
    }
}
