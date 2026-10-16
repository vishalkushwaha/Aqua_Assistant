//! Laya LearningRouter wrapper with LAYA_HOME pinning and safe fallback.

use super::fallback::{apply_decision_rules, fallback_decision};
use super::questions::{aqua_route_questions_json, aqua_safety_questions_json};
use super::{GateDecision, GateRoute, RawGateScores, SurfaceHint};
use indexmap::IndexMap;
use laya::{
    parse_questions, Answer, LearnOptions, LearningRouter, Questions, RouteOverrides, RouterOptions,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tracing::{info, warn};

/// Upper bound on concurrent `laya-decide` worker threads (including timed-out ones still running).
const MAX_LAYA_WORKERS: usize = 4;
static LAYA_WORKERS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Front-door gatekeeper. Never panics the pipeline — degrades to heuristics if Laya is off.
pub struct LayaGate {
    enabled: bool,
    ready: bool,
    router: Option<Mutex<LearningRouter>>,
    safety_questions: Option<Questions>,
    route_questions: Option<Questions>,
    laya_home: std::path::PathBuf,
    last_decision: Mutex<Option<GateDecision>>,
}

impl LayaGate {
    /// Heuristic-only gate (no model load). Used when `AQUA_LAYA=0` or in tests.
    pub fn disabled_heuristic(laya_home: PathBuf) -> Self {
        Self {
            enabled: false,
            ready: false,
            router: None,
            safety_questions: None,
            route_questions: None,
            laya_home,
            last_decision: Mutex::new(None),
        }
    }

    /// Set `LAYA_HOME` to `models_dir/laya`, then load LearningRouter when enabled.
    pub fn bootstrap(models_dir: &Path) -> Self {
        let laya_home = models_dir.join("laya");
        let _ = std::fs::create_dir_all(laya_home.join("models"));
        let _ = std::fs::create_dir_all(laya_home.join("learning"));

        std::env::set_var("LAYA_HOME", &laya_home);

        // Off unless `AQUA_LAYA=1`. The model took 4 to 80 seconds per request (often past its 15 s limit, after which the rules
        // below decided anyway) and its background inference kept the CPU busy for the language model. The rule gate answers in
        // microseconds: actions and live lookups (news, weather, web search) go to the agent, known attack phrasing is refused.
        let enabled = env_flag_enabled("AQUA_LAYA", false);
        if !enabled {
            info!("Laya model gate is off (set AQUA_LAYA=1 to use it) — using the rule gate");
            return Self::disabled_heuristic(laya_home);
        }

        match Self::try_init(&laya_home) {
            Ok((router, safety_questions, route_questions)) => {
                info!(
                    "Laya gate ready (LAYA_HOME={})",
                    laya_home.display()
                );
                Self {
                    enabled: true,
                    ready: true,
                    router: Some(Mutex::new(router)),
                    safety_questions: Some(safety_questions),
                    route_questions: Some(route_questions),
                    laya_home,
                    last_decision: Mutex::new(None),
                }
            }
            Err(e) => {
                warn!("Laya gate init failed — heuristic fallback: {e:#}");
                Self {
                    enabled: true,
                    ready: false,
                    router: None,
                    safety_questions: None,
                    route_questions: None,
                    laya_home,
                    last_decision: Mutex::new(None),
                }
            }
        }
    }

    fn try_init(laya_home: &Path) -> anyhow::Result<(LearningRouter, Questions, Questions)> {
        let safety_questions = parse_questions(&aqua_safety_questions_json())
            .map_err(|e| anyhow::anyhow!("parse safety questions: {e}"))?;
        let route_questions = parse_questions(&aqua_route_questions_json())
            .map_err(|e| anyhow::anyhow!("parse route questions: {e}"))?;

        let learn_opts = LearnOptions {
            state_path: Some(laya_home.join("learning").join("learned.json")),
            ..LearnOptions::default()
        };
        let router_opts = RouterOptions {
            max_loaded: 2,
            default: "english".into(),
            ..RouterOptions::default()
        };
        let router = LearningRouter::new(router_opts, learn_opts)
            .map_err(|e| anyhow::anyhow!("LearningRouter::new: {e}"))?;

        if let Err(e) = router.router().preload(Some(&["english"])) {
            warn!("Laya english preload skipped: {e}");
        }

        Ok((router, safety_questions, route_questions))
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn laya_home(&self) -> &Path {
        &self.laya_home
    }

    pub fn last_decision(&self) -> Option<GateDecision> {
        self.last_decision
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    pub fn health_detail(&self) -> String {
        if !self.enabled {
            return "disabled (AQUA_LAYA=0)".into();
        }
        if self.ready {
            format!("ready LAYA_HOME={}", self.laya_home.display())
        } else {
            format!(
                "fallback heuristics (init failed or models missing) LAYA_HOME={}",
                self.laya_home.display()
            )
        }
    }

    pub fn status_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "ready": self.ready,
            "laya_home": self.laya_home.display().to_string(),
            "detail": self.health_detail(),
            "last_decision": self.last_decision(),
        })
    }

    /// Decide with a wall-clock budget. Emits `on_tick(elapsed_secs)` every ~2s while waiting.
    /// On timeout, returns heuristic fallback immediately (debug Laya can take a minute+).
    /// A background decide may still finish and update `last_decision` afterward.
    pub fn decide_with_timeout(
        self: &std::sync::Arc<Self>,
        message: &str,
        mcp_available: bool,
        limit: std::time::Duration,
        mut on_tick: impl FnMut(u64),
    ) -> GateDecision {
        let message_owned = message.to_string();

        // Timed-out workers keep running (and queue on the router mutex). Cap them so a burst of
        // slow requests can't pile up unbounded threads; over the cap, answer with the heuristic.
        if LAYA_WORKERS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_LAYA_WORKERS {
            LAYA_WORKERS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            warn!("Laya gate busy ({MAX_LAYA_WORKERS} decisions in flight) — heuristic fallback");
            return crate::laya_validator::fallback_decision(&message_owned, mcp_available, "busy");
        }
        struct InFlight;
        impl Drop for InFlight {
            fn drop(&mut self) {
                LAYA_WORKERS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let slot = InFlight;

        let gate = std::sync::Arc::clone(self);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_msg = message_owned.clone();
        let spawned = std::thread::Builder::new()
            .name("laya-decide".into())
            .spawn(move || {
                let _slot = slot;
                let d = gate.decide(&worker_msg, mcp_available);
                let _ = tx.send(d);
            });
        if let Err(e) = spawned {
            warn!("could not spawn laya-decide thread ({e}) — heuristic fallback");
            return crate::laya_validator::fallback_decision(&message_owned, mcp_available, "fallback");
        }

        let started = Instant::now();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(d) => return d,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let secs = started.elapsed().as_secs();
                    on_tick(secs);
                    if started.elapsed() >= limit {
                        warn!(
                            "Laya gate exceeded {:?} — using heuristic fallback (inference still running in background)",
                            limit
                        );
                        return crate::laya_validator::fallback_decision(
                            &message_owned,
                            mcp_available,
                            "timeout",
                        );
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    warn!("Laya decide worker exited without a result — heuristic fallback");
                    return crate::laya_validator::fallback_decision(
                        &message_owned,
                        mcp_available,
                        "fallback",
                    );
                }
            }
        }
    }

    /// Env `AQUA_LAYA_TIMEOUT_SECS` (default 15).
    pub fn gate_timeout() -> std::time::Duration {
        let secs = std::env::var("AQUA_LAYA_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(15)
            .clamp(3, 120);
        std::time::Duration::from_secs(secs)
    }

    /// Decide refuse / chat / mcp_agent for `message`.
    /// Safety questions run first (isolated); routing questions second.
    pub fn decide(&self, message: &str, mcp_available: bool) -> GateDecision {
        let started = Instant::now();
        let message = message.trim();
        let mut decision = if message.is_empty() {
            GateDecision {
                route: GateRoute::Chat,
                needs_tools: false,
                jailbreak: 0.0,
                prompt_injection: 0.0,
                harm_severity: 0.0,
                confidence: 1.0,
                surface: SurfaceHint::Either,
                source: "empty".into(),
                latency_ms: 0,
            }
        } else if fast_path_enabled() && crate::laya_validator::fast_chat(message) {
            // Plain conversation: nothing in it can reach a tool, so the slow model check is skipped (see `fast_chat`).
            GateDecision {
                route: GateRoute::Chat,
                needs_tools: false,
                jailbreak: 0.0,
                prompt_injection: 0.0,
                harm_severity: 0.0,
                confidence: 0.9,
                surface: SurfaceHint::Either,
                source: "fast".into(),
                latency_ms: 0,
            }
        } else if !self.enabled {
            fallback_decision(message, mcp_available, "disabled")
        } else if self.router.is_none()
            || self.safety_questions.is_none()
            || self.route_questions.is_none()
        {
            fallback_decision(message, mcp_available, "fallback")
        } else {
            match self.predict_two_pass(message, mcp_available) {
                Ok(d) => d,
                Err(e) => {
                    warn!("Laya two-pass predict failed — fallback: {e}");
                    fallback_decision(message, mcp_available, "fallback")
                }
            }
        };

        decision.latency_ms = started.elapsed().as_millis() as u64;
        // The pipeline logs the final decision at INFO; this is the model's own, before the rules change it.
        tracing::debug!("Laya gate {}", decision.log_fields());
        if let Ok(mut slot) = self.last_decision.lock() {
            *slot = Some(decision.clone());
        }
        decision
    }

    fn predict_two_pass(
        &self,
        message: &str,
        mcp_available: bool,
    ) -> anyhow::Result<GateDecision> {
        let router = self.router.as_ref().unwrap();
        let safety_q = self.safety_questions.as_ref().unwrap();
        let route_q = self.route_questions.as_ref().unwrap();
        let guard = router
            .lock()
            .map_err(|e| anyhow::anyhow!("laya lock: {e}"))?;

        // Guard-style state key so safety heads see a `prompt` field.
        let safety_state = json!({ "prompt": message });
        let safety_pred = guard
            .predict(&safety_state, safety_q, &RouteOverrides::default())
            .map_err(|e| anyhow::anyhow!("safety predict: {e}"))?;
        let mut raw = extract_scores(&safety_pred.answers);

        let route_state = json!({ "message": message });
        let route_pred = guard
            .predict(&route_state, route_q, &RouteOverrides::default())
            .map_err(|e| anyhow::anyhow!("route predict: {e}"))?;
        let route_raw = extract_scores(&route_pred.answers);

        raw.route_choice = route_raw.route_choice;
        raw.needs_tools = route_raw.needs_tools;
        raw.surface_choice = route_raw.surface_choice;
        raw.confidence = (raw.confidence + route_raw.confidence) / 2.0;

        Ok(apply_decision_rules(&raw, message, mcp_available, "laya"))
    }

    /// Teach the LearningRouter the correct `route` label for a past request.
    pub fn feedback_route(&self, message: &str, correct: GateRoute) {
        let Some(router) = self.router.as_ref() else {
            return;
        };
        let Some(route_questions) = self.route_questions.as_ref() else {
            return;
        };
        let state = json!({ "message": message.trim() });
        let mut correct_map = IndexMap::new();
        correct_map.insert("route".into(), Value::String(correct.as_str().into()));
        correct_map.insert(
            "needs_tools".into(),
            Value::Bool(correct == GateRoute::McpAgent),
        );

        let Ok(guard) = router.lock() else {
            return;
        };
        match guard.feedback(&state, route_questions, &correct_map, Some("en")) {
            Ok(_) => {
                let _ = guard.persist();
                info!(
                    "Laya feedback recorded route={} for {:?}",
                    correct.as_str(),
                    truncate(message, 80)
                );
            }
            Err(e) => warn!("Laya feedback failed: {e}"),
        }
    }
}

fn extract_scores(answers: &IndexMap<String, Answer>) -> RawGateScores {
    let mut raw = RawGateScores {
        route_choice: None,
        needs_tools: 0.0,
        jailbreak: 0.0,
        prompt_injection: 0.0,
        harm_severity: 0.0,
        confidence: 0.0,
        surface_choice: None,
    };
    let mut conf_sum = 0.0;
    let mut conf_n = 0.0;

    for (key, ans) in answers {
        conf_sum += ans.confidence();
        conf_n += 1.0;
        match (key.as_str(), ans) {
            ("route", Answer::Choice { choice, .. }) => {
                raw.route_choice = Some(choice.clone());
            }
            ("needs_tools", Answer::Noul { noul, .. }) => raw.needs_tools = *noul,
            ("jailbreak", Answer::Noul { noul, .. }) => raw.jailbreak = *noul,
            ("prompt_injection", Answer::Noul { noul, .. }) => raw.prompt_injection = *noul,
            ("harm_severity", Answer::Score { score, .. }) => raw.harm_severity = *score,
            ("surface", Answer::Choice { choice, .. }) => {
                raw.surface_choice = Some(choice.clone());
            }
            _ => {}
        }
    }
    raw.confidence = if conf_n > 0.0 { conf_sum / conf_n } else { 0.0 };
    raw
}

fn env_flag_enabled(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        }
        Err(_) => default,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut t = s.chars().take(max).collect::<String>();
    if s.chars().count() > max {
        t.push('…');
    }
    t
}

/// `AQUA_LAYA_FAST=0` turns the shortcut for plain conversation off (every message then goes through the model).
fn fast_path_enabled() -> bool {
    !matches!(std::env::var("AQUA_LAYA_FAST").ok().as_deref().map(str::to_ascii_lowercase).as_deref(), Some("0" | "off" | "false" | "no"))
}
