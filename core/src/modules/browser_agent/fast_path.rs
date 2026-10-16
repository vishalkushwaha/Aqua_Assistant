//! A recipe learned from an earlier run, replayed with no planning round, and what is kept of a run as a recipe, with no typed text and no secret in it (card OA3-05, move only).

use super::*;

/// Does this step of a run change something (write, click, type, open, send)? Recipes of version 2 say so for each step; when unsure it counts as a change.
pub(super) fn recipe_step_writes(source: &str, tool: &str, arguments: &Value) -> bool {
    match source {
        "host" => crate::agent_tools::can_change_something(crate::agent_tools::host().map(|h| &**h), tool),
        "browser" | "windows" => acts_on_the_world(source, tool, arguments),
        "local" => !tool.eq_ignore_ascii_case("local_find"),
        _ => true,
    }
}

pub(super) fn recipe_from_steps(steps: &[AgentStep], final_answer: &str) -> LearnedRecipe {
    // With AQUA_RECIPES_V2 off, what is made is exactly what it was: no flag, no hash of a hidden text, no postcondition.
    let v2 = crate::memory::agent_learning::recipes_v2_enabled();
    let mut tools = Vec::new();
    let mut postconditions = Vec::new();
    for step in steps {
        if step.kind != "tool" && step.kind != "fast_path" {
            continue;
        }
        let tool = step
            .detail
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if tool.is_empty() {
            continue;
        }
        // The loop's own tools change nothing outside the run, and their arguments are text the model wrote (a checklist): a recipe keeps neither (card OA5-01).
        if matches!(tool.as_str(), "todo.write" | "agent.explore" | "agent.note" | "agent.task") {
            continue;
        }
        let raw = step.detail.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let arguments = redact_recipe_args(&tool, &raw);
        let mut call = LearnedToolCall {
            tool,
            arguments,
            source: step
                .detail
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            ..Default::default()
        };
        if v2 {
            // Which texts redaction hid (a hash of each, to tell text the goal quoted from text the planner wrote), whether the step changes something, and what must be
            // true after it, checked against this run's own result.
            call.hidden = crate::memory::agent_learning::hidden_leaves(&raw, &call.arguments);
            call.write = recipe_step_writes(&call.source, &call.tool, &raw);
            let observation = step.detail.get("observation").and_then(|v| v.as_str()).unwrap_or("");
            postconditions.extend(crate::memory::agent_learning::infer_postconditions(tools.len(), &call.tool, &raw, observation, &|p: &str| std::path::Path::new(p).exists()));
        }
        tools.push(call);
    }
    // One page check is enough: the last page the recipe opened (an earlier one may redirect from one run to the next).
    if let Some(last) = postconditions.iter().rposition(|p| matches!(p, crate::memory::agent_learning::Postcondition::ResultContains { .. })) {
        let mut at = 0usize;
        postconditions.retain(|p| {
            let keep = !matches!(p, crate::memory::agent_learning::Postcondition::ResultContains { .. }) || at == last;
            at += 1;
            keep
        });
    }
    postconditions.truncate(8);
    LearnedRecipe {
        tools,
        final_answer: final_answer.to_string(),
        postconditions,
        ..Default::default()
    }
}

/// Steps a recipe may replay without the planner: navigation and reads only. Navigation still
/// passes the browser policy (site rules, confirmations) on every replay.
pub(super) fn is_replay_safe(tool: &str) -> bool {
    aqua_mcp_adapters::browser::policy::is_idempotent(tool)
        || matches!(tool, "Snapshot" | "Screenshot" | "Wait" | "WaitFor" | "DisplayInventory")
}

/// Replace typed text and credential-looking URL parameters so recipes never store secrets.
pub fn redact_recipe_args(tool: &str, args: &Value) -> Value {
    if tool.contains('.') && !tool.starts_with("browser") {
        return crate::agent_tools::redact_args(args);
    }
    let mut a = args.clone();
    let hide = |v: &mut Value| {
        if let Some(t) = v.as_str() {
            *v = Value::String(format!("<redacted {} chars>", t.chars().count()));
        }
    };
    let acts_on_text = matches!(tool, "Type" | "MultiEdit" | "browser_type" | "browser_fill_form" | "browser_press_key");
    if let Some(obj) = a.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            let kl = k.to_ascii_lowercase();
            let secret_key = ["password", "passwd", "secret", "token", "apikey", "api_key", "credential"]
                .iter()
                .any(|f| kl.contains(f));
            if secret_key || (acts_on_text && matches!(kl.as_str(), "text" | "value" | "key")) {
                hide(v);
            } else if kl == "fields" {
                if let Some(arr) = v.as_array_mut() {
                    for f in arr {
                        if let Some(val) = f.get_mut("value") {
                            hide(val);
                        }
                    }
                }
            } else if kl == "url" {
                if let Some(u) = v.as_str() {
                    *v = Value::String(redact_url_secrets(u));
                }
            }
        }
    }
    a
}

/// Blank out the value of query/fragment parameters whose name suggests a credential.
pub(super) fn redact_url_secrets(url: &str) -> String {
    let (base, frag) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let scrub_pairs = |pairs: &str| -> String {
        pairs
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _))
                    if ["token", "key", "secret", "password", "passwd", "auth", "sig", "signature", "code", "session", "sid", "apikey", "access"]
                        .iter()
                        .any(|f| k.to_ascii_lowercase().contains(f))
                        // who a message goes to and what it says are personal, whatever the site calls the parameter
                        || ["to", "cc", "bcc", "body", "subject", "su", "message", "msg", "text"].contains(&k.to_ascii_lowercase().as_str()) =>
                {
                    format!("{k}=<redacted>")
                }
                _ => kv.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&")
    };
    let mut out = match base.split_once('?') {
        Some((path, q)) => format!("{path}?{}", scrub_pairs(q)),
        None => base.to_string(),
    };
    if let Some(f) = frag {
        // Fragments carry OAuth tokens (`#access_token=…`); keep them only when they are plain anchors.
        if f.contains('=') {
            out.push_str(&format!("#{}", scrub_pairs(f)));
        } else {
            out.push_str(&format!("#{f}"));
        }
    }
    out
}

/// Replay a previously successful recipe without LLM planning.
pub(super) fn try_learned_fast_path(
    learning: &AgentLearning,
    ctx: &crate::memory::AgentLearningContext,
    tools: &[McpTool],
    browser: &Option<BrowserHandle>,
    windows: &Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
) -> Option<McpAgentResult> {
    let matched = ctx.matched.as_ref()?;
    if !matched.is_fast_path_eligible {
        return None;
    }
    // Don't replay recipes that have been failing recently.
    if matched.solution.success_rate < 0.5 && matched.solution.execution_count > 2 {
        return None;
    }
    let recipe = ctx.recipe.as_ref()?;
    if recipe.tools.is_empty() {
        return None;
    }
    // A recipe of version 2 is for `recipe_replay` (AQUA_RECIPES_V2): replayed here, its placeholders (`{{file1}}`) would be sent as if they were values.
    if recipe.version >= 2 {
        return None;
    }
    // A stored recipe is replayed without the LLM and without a fresh look at the goal, so it
    // may only contain steps that read. Anything that acts (click, type, launch, send…) is
    // left to the planner, which goes through the guard and asks the user.
    if let Some(unsafe_step) = recipe.tools.iter().find(|c| !is_replay_safe(&c.tool)) {
        info!("Learned recipe not replayed: '{}' is not a read-only step", unsafe_step.tool);
        return None;
    }

    info!(
        "MCP agent learned fast path (sim={:.2}, solution_id={})",
        matched.similarity_score, matched.solution.id
    );

    let mut steps = vec![AgentStep {
        step: 0,
        kind: "learned_lookup".into(),
        detail: json!({
            "solution_id": matched.solution.id,
            "similarity": matched.similarity_score,
            "intent": matched.solution.intent_pattern,
        }),
    }];
    let mut history: Vec<(String, String)> = Vec::new();
    let mut last_obs = String::new();
    for (i, call) in recipe.tools.iter().enumerate() {
        match call_named_tool(
            &call.tool,
            call.arguments.clone(),
            tools,
            browser,
            windows,
        ) {
            Ok((label, obs)) => {
                if obs.starts_with("ERROR") {
                    warn!("Learned recipe tool {} failed: {obs}", call.tool);
                    learning.mark_recipe_reused(matched.solution.id, false);
                    let _ = learning.on_tool_error(
                        &matched.solution.intent_pattern,
                        &call.tool,
                        &obs,
                    );
                    return None;
                }
                last_obs = obs.clone();
                steps.push(AgentStep {
                    step: (i + 1) as u32,
                    kind: "tool".into(),
                    detail: json!({
                        "tool": call.tool,
                        "source": label,
                        "arguments": call.arguments,
                        "observation": record_observation(&obs),
                    }),
                });
                history.push((call.tool.clone(), obs.clone()));
                if (is_action_ack_only(&obs) || call.tool.eq_ignore_ascii_case("App"))
                    && !is_observe_tool(&call.tool)
                {
                    if let Some(auto_obs) = maybe_auto_observe(tools, browser, windows, &call.tool) {
                        steps.push(AgentStep {
                            step: (i + 1) as u32,
                            kind: "auto_observe".into(),
                            detail: json!({
                                "tool": auto_obs.0,
                                "source": auto_obs.1,
                                "observation": record_observation(&auto_obs.2),
                            }),
                        });
                        history.push((auto_obs.0, auto_obs.2));
                    }
                }
            }
            Err(e) => {
                warn!("Learned recipe replay error: {e:#}");
                learning.mark_recipe_reused(matched.solution.id, false);
                return None;
            }
        }
    }

    // Live data (weather, news, prices) must come from what was just read. When the fresh reading
    // cannot be turned into a clean answer, the planner summarises it; the saved answer is never used.
    if crate::laya_validator::looks_live_info(goal) && format_live_observation(&last_obs).is_none() {
        info!("Learned fast path skipped: a live-data goal has no clean fresh reading; the planner will read it");
        return None;
    }

    let answer = if recipe.final_answer.split_whitespace().count() >= 10 {
        let live = format_live_observation(&last_obs).unwrap_or_default();
        // A saved final answer is from an earlier run: for live data (news, prices…) or list
        // results it is stale, so the fresh observation replaces it instead of being appended.
        if !live.is_empty()
            && (crate::laya_validator::looks_live_info(goal) || live.contains('\n'))
        {
            live
        } else if live.is_empty() {
            recipe.final_answer.clone()
        } else {
            format!("{} (latest reading: {})", recipe.final_answer.trim(), live)
        }
    } else if !last_obs.is_empty() {
        format_live_observation(&last_obs).unwrap_or_else(|| {
            if !recipe.final_answer.trim().is_empty() {
                recipe.final_answer.clone()
            } else {
                last_obs.clone()
            }
        })
    } else if !recipe.final_answer.trim().is_empty() {
        recipe.final_answer.clone()
    } else {
        learning.mark_recipe_reused(matched.solution.id, false);
        return None;
    };

    if answer.trim().is_empty() {
        learning.mark_recipe_reused(matched.solution.id, false);
        return None;
    }

    // Whatever produced it (the saved answer or a formatted fresh reading), text that is really a
    // page's element list or a desktop snapshot is not an answer: the planner reads it instead.
    if crate::memory::solutions_db::looks_like_raw_dump(&answer) {
        info!("Learned fast path skipped: the result is a raw page dump, not an answer; the planner will read it");
        return None;
    }

    // Incomplete recipes (e.g. only "Chrome launched") must not short-circuit multi-part goals.
    if let Some(reason) = reject_premature_final(goal, &answer, &history) {
        warn!("Learned fast path incomplete for current goal ({reason}); falling through to LLM");
        learning.mark_recipe_reused(matched.solution.id, false);
        return None;
    }

    steps.push(AgentStep {
        step: (recipe.tools.len() + 1) as u32,
        kind: "final".into(),
        detail: json!({ "answer": answer }),
    });

    learning.mark_recipe_reused(matched.solution.id, true);
    let store_final = if recipe.final_answer.split_whitespace().count()
        > answer.split_whitespace().count()
    {
        recipe.final_answer.clone()
    } else {
        answer.clone()
    };
    learning.learn_success(
        goal,
        &recipe_from_steps(&steps, &store_final),
    );

    Some(McpAgentResult {
        final_answer: answer,
        steps,
        tool_rounds: recipe.tools.len() as u32,
        stopped_reason: "learned_fast_path".into(),
        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
    })
}
