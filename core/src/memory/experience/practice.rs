//! Practice tasks the agent generates for itself.
//!
//! When it has been idle for a while, the agent can make up a few small tasks in the kinds you already
//! use, run them like any other task and learn from what it finds. Each one either strengthens an
//! experience (a rewording of something it already knows, which tests that retrieval and reuse work) or
//! adds a new one (the same kind of task about something new: another asset, another city).
//!
//! Tasks are built from templates and short allow-lists, not invented freely by the model: a small
//! local model making up tasks for itself would just as easily teach itself wrong things. Every
//! generated goal must also pass a read-only filter: no actions, no accounts, no addresses, no
//! secrets. Practice runs unattended (see `browser::unattended`), so anything that would need an
//! approval is refused on the spot, and they get no file, terminal or other host tools.
//!
//! It is off by default, needs the learning master switch, is limited to a few tasks a day with a
//! minimum gap between them, and starts only when nobody has used the agent for a while.

use super::model::{now_secs, Experience};
use super::sanitize::redact;
use super::service::LearningService;
use super::settings::LearningSettings;
use super::store::{ListFilter, PracticeRow};
use serde::{Deserialize, Serialize};
use std::panic::{catch_unwind, AssertUnwindSafe};

const DAY: i64 = 86_400;
/// After a refused practice (it needed an approval), wait this long before trying again.
const REFUSED_BACKOFF_SECS: i64 = 6 * 3600;
/// A goal practised in this window is not generated again.
const NO_REPEAT_DAYS: i64 = 30;

/// A kind of information task: how it is worded and what it can be about.
pub struct Family {
    pub name: &'static str,
    /// Words in a past task that show you use this kind.
    pub keywords: &'static [&'static str],
    /// `{e}` is replaced by an entity.
    pub templates: &'static [&'static str],
    pub entities: &'static [&'static str],
}

pub const FAMILIES: &[Family] = &[
    Family {
        name: "crypto_price",
        keywords: &["bitcoin", "btc", "ethereum", "crypto", "solana", "dogecoin"],
        templates: &["{e} price today", "what is the {e} price today"],
        entities: &["bitcoin", "ethereum", "solana", "dogecoin", "xrp"],
    },
    Family {
        name: "commodity_price",
        keywords: &["gold", "silver", "crude", "oil", "petrol", "diesel"],
        templates: &["{e} price today", "what is the {e} price today in india"],
        entities: &["gold", "silver", "crude oil", "petrol", "diesel"],
    },
    Family {
        name: "stock_market",
        keywords: &["stock", "market", "sensex", "nifty", "nasdaq", "dow", "shares"],
        templates: &["what is the {e} condition today", "tell me today {e} condition"],
        entities: &["stock market", "nifty 50", "sensex", "nasdaq", "dow jones", "stock exchange"],
    },
    Family {
        name: "weather",
        keywords: &["weather", "forecast", "temperature"],
        templates: &["what is the weather in {e} today", "tell me today's {e} weather"],
        entities: &["Pune", "Mumbai", "Delhi", "Bengaluru", "Chennai", "Kolkata", "Jaipur", "Bikaner"],
    },
    Family {
        name: "news",
        keywords: &["news", "headlines", "headline"],
        templates: &["top 3 news headlines today", "what is the latest {e} news today"],
        entities: &["india", "technology", "business", "sports", "world"],
    },
    Family {
        name: "sports_score",
        keywords: &["cricket", "score", "match", "ipl", "football"],
        templates: &["latest {e} match score", "what is the latest {e} score today"],
        entities: &["cricket", "football", "tennis", "hockey"],
    },
];

/// Words that make a goal an action instead of a lookup. A practice goal containing any of them is
/// refused, whatever else is true of it.
const ACTION_WORDS: &[&str] = &[
    "send", "email", "mail", "message", "delete", "remove", "erase", "buy", "purchase", "pay", "order", "book", "reserve",
    "install", "uninstall", "run", "execute", "open", "launch", "start", "stop", "click", "type", "login", "log", "sign",
    "post", "submit", "download", "upload", "transfer", "write", "create", "edit", "save", "reply", "share", "subscribe",
    "unsubscribe", "cancel", "password", "passwords", "token", "key", "secret", "account", "my", "file", "folder",
    "terminal", "command", "shell", "sudo",
];

/// A practice goal must be a short, plain, read-only information request.
pub fn is_safe_practice_goal(goal: &str) -> bool {
    let g = goal.trim();
    if g.len() < 8 || g.len() > 120 {
        return false;
    }
    if !g.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '\'' | '-' | ',' | '.')) {
        return false; // no URLs, addresses, paths, symbols
    }
    let lower = g.to_ascii_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.iter().any(|w| ACTION_WORDS.contains(w)) {
        return false;
    }
    if redact(g).1 > 0 {
        return false;
    }
    crate::laya_validator::looks_live_info(g)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratedTask {
    pub goal: String,
    pub family: String,
    /// `variation` or `paraphrase`.
    pub source: String,
}

/// The kinds of task you already use: families whose keywords appear in a usable experience.
pub fn interest_families(problems: &[String]) -> Vec<&'static Family> {
    FAMILIES
        .iter()
        .filter(|f| {
            problems.iter().any(|p| {
                let lower = p.to_ascii_lowercase();
                let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
                f.keywords.iter().any(|k| words.contains(k))
            })
        })
        .collect()
}

/// Reword a past task: same question, different phrasing, so retrieval and reuse get exercised.
fn paraphrases_of(problem: &str) -> Vec<String> {
    let mut core = problem.trim().to_ascii_lowercase();
    for lead in ["tell me ", "what is ", "what's ", "how is ", "give me ", "show me "] {
        if let Some(rest) = core.strip_prefix(lead) {
            core = rest.to_string();
            break;
        }
    }
    let core = core
        .split_whitespace()
        .filter(|w| !matches!(*w, "today" | "todays" | "today's" | "the" | "right" | "now"))
        .collect::<Vec<_>>()
        .join(" ");
    if core.split_whitespace().count() < 2 {
        return Vec::new();
    }
    vec![format!("what is the {core} today"), format!("tell me the {core} right now"), format!("{core} today")]
}

fn family_of(problem: &str) -> &'static str {
    interest_families(&[problem.to_string()]).first().map(|f| f.name).unwrap_or("general_live")
}

/// Make up one practice task. Deterministic for a given `seed` (so it can be tested); `problems` are
/// the usable experiences' problem statements, `recent_goals` what was practised lately.
pub fn generate(problems: &[String], recent_goals: &[String], seed: u64) -> Option<GeneratedTask> {
    let families = interest_families(problems);
    if families.is_empty() {
        return None; // nothing to practise until the agent has verified something worth practising
    }
    let taken: Vec<String> = problems.iter().chain(recent_goals.iter()).map(|g| g.trim().to_ascii_lowercase()).collect();
    let fresh = |goal: &str| !taken.contains(&goal.trim().to_ascii_lowercase()) && is_safe_practice_goal(goal);

    let mut variations: Vec<(&'static str, String)> = Vec::new();
    for f in &families {
        for t in f.templates {
            for e in f.entities {
                let goal = t.replace("{e}", e);
                if fresh(&goal) && !variations.iter().any(|(_, g)| g.eq_ignore_ascii_case(&goal)) {
                    variations.push((f.name, goal));
                }
            }
        }
    }
    let mut paraphrases: Vec<(&'static str, String)> = Vec::new();
    for p in problems {
        for goal in paraphrases_of(p) {
            if fresh(&goal) && !paraphrases.iter().any(|(_, g)| g.eq_ignore_ascii_case(&goal)) {
                paraphrases.push((family_of(p), goal));
            }
        }
    }
    let pick = |list: &Vec<(&'static str, String)>, seed: u64| list[(seed as usize) % list.len()].clone();
    // One in three is a rewording of something already known; the rest are new tasks of a known kind.
    let (source, (family, goal)) = if seed.is_multiple_of(3) && !paraphrases.is_empty() {
        ("paraphrase", pick(&paraphrases, seed / 3))
    } else if !variations.is_empty() {
        ("variation", pick(&variations, seed))
    } else if !paraphrases.is_empty() {
        ("paraphrase", pick(&paraphrases, seed))
    } else {
        return None;
    };
    Some(GeneratedTask { goal, family: family.to_string(), source: source.to_string() })
}

// ---- when may a practice task start ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Skip {
    Disabled,
    UserActive,
    NotIdle { idle_mins: i64, needs_mins: u32 },
    DailyLimit { limit: u32 },
    TooSoon { mins_left: i64 },
    /// The last practice needed an approval it cannot give; not retried for a few hours.
    NeedsApproval { hours_left: i64 },
    NothingToPractice,
    Error(String),
}

impl Skip {
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => "auto practice is off".into(),
            Self::UserActive => "the agent is busy with your task".into(),
            Self::NotIdle { idle_mins, needs_mins } => format!("waiting for {needs_mins} idle minutes ({idle_mins} so far)"),
            Self::DailyLimit { limit } => format!("today's limit of {limit} practice task(s) is used up"),
            Self::TooSoon { mins_left } => format!("next practice in {mins_left} minute(s)"),
            Self::NeedsApproval { hours_left } => format!("the last practice needed an approval it cannot give; retrying in about {hours_left} hour(s). Run a web task yourself once and choose Always allow for the site"),
            Self::NothingToPractice => "nothing to practise yet: needs a verified experience of a kind it can practise".into(),
            Self::Error(e) => format!("practice could not start: {e}"),
        }
    }
}

/// What the caller knows about the outside world when asking.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub now: i64,
    /// Unix seconds of the last user task, or 0 if none since start.
    pub last_user_activity: i64,
    /// A user task is running right now.
    pub user_active: bool,
    /// "Run one now" from the UI: skips the idle wait and the gap, nothing else.
    pub force: bool,
}

pub fn check(settings: &LearningSettings, started_last_24h: u32, last_started: i64, gate: &Gate) -> Result<(), Skip> {
    if !settings.may_practice() {
        return Err(Skip::Disabled);
    }
    if gate.user_active {
        return Err(Skip::UserActive);
    }
    if started_last_24h >= settings.practice_max_per_day {
        return Err(Skip::DailyLimit { limit: settings.practice_max_per_day });
    }
    if !gate.force {
        let idle_mins = if gate.last_user_activity <= 0 { i64::MAX / 120 } else { (gate.now - gate.last_user_activity).max(0) / 60 };
        if idle_mins < settings.practice_idle_mins as i64 {
            return Err(Skip::NotIdle { idle_mins, needs_mins: settings.practice_idle_mins });
        }
        let since_last = (gate.now - last_started).max(0);
        let gap = settings.practice_min_interval_mins as i64 * 60;
        if last_started > 0 && since_last < gap {
            return Err(Skip::TooSoon { mins_left: (gap - since_last + 59) / 60 });
        }
    }
    Ok(())
}

// ---- running one ---------------------------------------------------------------------------------

/// What happened when a practice task ran. `status` is `verified`, `unverified`, `interrupted`,
/// `refused` (it needed an approval) or `failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PracticeOutcome {
    pub status: String,
    pub verified: bool,
    pub steps: u32,
    pub note: String,
}

impl PracticeOutcome {
    pub fn failed(note: impl Into<String>) -> Self {
        Self { status: "failed".into(), verified: false, steps: 0, note: note.into() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TickResult {
    Skipped(Skip),
    Ran { id: i64, task: GeneratedTask, outcome: PracticeOutcome },
}

fn usable_problems(experiences: &[Experience]) -> Vec<String> {
    experiences
        .iter()
        .filter(|e| e.status.is_usable() && e.task_type == "live_data")
        .map(|e| e.problem_summary.clone())
        .collect()
}

/// Decide whether to practise now and, if so, generate one task and run it with `runner`. The runner
/// does the actual agent run (see `modules::practice_runner`); it is a parameter so this can be tested
/// without a browser or a model. Never panics and never fails the caller.
pub fn tick<F>(svc: &LearningService, gate: &Gate, runner: F) -> TickResult
where
    F: FnOnce(&GeneratedTask, &LearningSettings) -> PracticeOutcome,
{
    let settings = svc.settings();
    let store = svc.store();
    let started = match store.practice_started_since(gate.now - DAY) {
        Ok(n) => n,
        Err(e) => return TickResult::Skipped(Skip::Error(e)),
    };
    if let Err(skip) = check(&settings, started, store.last_practice_started(), gate) {
        return TickResult::Skipped(skip);
    }
    if let Some(skip) = refused_backoff(svc, gate) {
        return TickResult::Skipped(skip);
    }
    let experiences = match store.list(&ListFilter { limit: Some(200), ..Default::default() }) {
        Ok(v) => v,
        Err(e) => return TickResult::Skipped(Skip::Error(e)),
    };
    let recent = store.practice_goals_since(gate.now - NO_REPEAT_DAYS * DAY).unwrap_or_default();
    let seed = (gate.now as u64).wrapping_add(started as u64 * 7919);
    let Some(task) = generate(&usable_problems(&experiences), &recent, seed) else {
        return TickResult::Skipped(Skip::NothingToPractice);
    };
    let id = match store.practice_start(&task.goal, &task.family, &task.source) {
        Ok(id) => id,
        Err(e) => return TickResult::Skipped(Skip::Error(e)),
    };
    store.log_event("practice_task_started", &serde_json::json!({ "id": id, "family": task.family, "source": task.source, "goal": task.goal }), None);
    let outcome = catch_unwind(AssertUnwindSafe(|| runner(&task, &settings))).unwrap_or_else(|_| PracticeOutcome::failed("the practice run panicked (contained)"));
    let _ = store.practice_finish(id, &outcome.status, outcome.verified, &outcome.note, outcome.steps);
    let _ = store.bump("practice_runs", 1.0);
    if outcome.verified {
        let _ = store.bump("practice_verified", 1.0);
    }
    store.log_event("practice_task_completed", &serde_json::json!({ "id": id, "status": outcome.status, "verified": outcome.verified }), None);
    TickResult::Ran { id, task, outcome }
}

/// A practice that was refused (it needed an approval) is not retried straight away; asking again
/// every hour would only be refused again. "Run one now" skips this, since it means you just acted.
fn refused_backoff(svc: &LearningService, gate: &Gate) -> Option<Skip> {
    if gate.force {
        return None;
    }
    let last = svc.store().practice_recent(1).ok()?.into_iter().next()?;
    let waited = gate.now - last.finished_at.max(last.started_at);
    (last.status == "refused" && waited < REFUSED_BACKOFF_SECS).then(|| Skip::NeedsApproval { hours_left: (REFUSED_BACKOFF_SECS - waited + 3599) / 3600 })
}

// ---- for the dashboard -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PracticeStatus {
    pub enabled: bool,
    pub max_per_day: u32,
    pub started_last_24h: u32,
    pub min_interval_mins: u32,
    pub idle_mins: u32,
    /// Unix seconds before which no practice starts because of the gap, or 0.
    pub next_allowed_at: i64,
    /// Why nothing is running right now, in words.
    pub waiting_for: String,
    pub recent: Vec<PracticeRow>,
}

pub fn status(svc: &LearningService, gate: &Gate) -> PracticeStatus {
    let settings = svc.settings();
    let store = svc.store();
    let started = store.practice_started_since(gate.now - DAY).unwrap_or(0);
    let last = store.last_practice_started();
    let waiting_for = match check(&settings, started, last, gate) {
        Ok(()) => match refused_backoff(svc, gate) {
            Some(skip) => skip.describe(),
            None => "ready: will start on the next check".to_string(),
        },
        Err(s) => s.describe(),
    };
    PracticeStatus {
        enabled: settings.may_practice(),
        max_per_day: settings.practice_max_per_day,
        started_last_24h: started,
        min_interval_mins: settings.practice_min_interval_mins,
        idle_mins: settings.practice_idle_mins,
        next_allowed_at: if last > 0 { last + settings.practice_min_interval_mins as i64 * 60 } else { 0 },
        waiting_for,
        recent: store.practice_recent(20).unwrap_or_default(),
    }
}

/// Mark practice tasks a crash left running as interrupted. Called once at start-up.
pub fn recover(svc: &LearningService) {
    if let Ok(n) = svc.store().interrupt_stale_practice() {
        if n > 0 {
            tracing::info!("marked {n} interrupted practice task(s)");
        }
    }
}

/// The current time, for callers that do not have a clock of their own.
pub fn now() -> i64 {
    now_secs()
}
