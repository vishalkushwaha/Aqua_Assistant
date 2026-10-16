//! Starting an app by name ("open Notepad"): fast, learned, and never twice.
//!
//! **Why this exists.** The only way Aqua started an app was through the Windows-MCP helper, which for one launch lists the
//! whole Start menu with one PowerShell, checks the app with a second, starts it with a third, and for Store apps then
//! searches the window tree for up to 10 seconds. That took 2 seconds on a quiet machine and 16 to 36 seconds while the
//! speech models were running. And while it ran, a person who saw nothing happen said the command again.
//!
//! **What it does now**, in this order, stopping at the first that starts the app:
//! 1. a recipe learned earlier for this name (a plain call to the shell, a few milliseconds);
//! 2. a program that ships with Windows (`notepad.exe`);
//! 3. the file index (a Start menu shortcut or a program found on the disk);
//! 4. the Start menu itself, read once with one PowerShell (a second or two) and remembered for ten minutes, which also reaches
//!    Store apps;
//! 5. the Windows-MCP helper, as before, only when nothing faster could.
//!
//! **Learning.** When a launch is seen to work (a new window of the right program appears), the recipe is rewarded; when it
//! fails, it is penalised, and after two failures in a row it is dropped so a new way is found. A name that was found in the
//! Start menu is remembered, so the next request skips the lookup. Learning follows the learning switch in the admin panel.
//!
//! **Never twice.** One launch of a name at a time: a second request for the same name while the first is still starting
//! waits for it and shares its answer, and one made within ten seconds of a launch whose window is still open is answered
//! "already open". After a start that Windows accepted, nothing else is ever started for the same request: if the window is
//! not seen in time, the answer says so instead of trying another way.

pub mod skills;
mod sys;

use crate::file_index::{global as file_index, LookupRequest, Want};
use crate::windows_mcp::guard::{evaluate, Caller, Verdict};
use serde_json::{json, Value};
use skills::{best_start_app, display, normalize, parse_start_apps, recipe_valid, well_known, Kind, Recipe, Skill, Store};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long to wait for the new window after starting something.
const SEE_WINDOW: Duration = Duration::from_millis(3000);
/// The same when a window of that program was open already (the program may just bring it forward).
const SEE_WINDOW_IF_OPEN: Duration = Duration::from_millis(1500);
/// How long the Start menu list is kept before it is read again.
const START_MENU_KEPT: Duration = Duration::from_secs(600);
/// A launch of the same name this soon after one whose window is still open is the same request again.
const DUPLICATE_WINDOW: Duration = Duration::from_secs(10);
/// The longest a second request waits for the first to finish.
const FOLLOW_WAIT: Duration = Duration::from_secs(60);

pub struct Options {
    /// Learn from this launch (the admin panel's learning switch).
    pub learn: bool,
    /// Use what was learned.
    pub use_learned: bool,
}

/// What the slow route (the Windows-MCP helper) reports, as far as it can be told from its answer.
pub enum Slow {
    /// It says the app was started.
    Opened,
    /// The guard refused it (the sentence to say).
    Denied(String),
    /// It did not start the app: not in the Start menu, the helper is not running. Nothing was launched.
    Failed,
    /// It did not answer in time or broke: the app may or may not have started, so nothing else is started.
    Unknown,
}

struct State {
    path: PathBuf,
    store: Store,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
type Sink = Box<dyn Fn(&str, Value) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        let path = skills::default_path();
        Mutex::new(State { store: Store::load(&path), path })
    })
}

/// Choose where the learned recipes are kept (once, at start-up).
pub fn init_global(path: PathBuf) {
    let _ = STATE.set(Mutex::new(State { store: Store::load(&path), path }));
}

/// Where learning events are written (the admin panel's activity log).
pub fn set_event_sink(f: impl Fn(&str, Value) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(f));
}

fn emit(event: &str, detail: Value) {
    if let Some(s) = SINK.get() {
        s(event, detail);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut g = state().lock().unwrap_or_else(|p| p.into_inner());
    let r = f(&mut g.store);
    let path = g.path.clone();
    if let Err(e) = g.store.save(&path) {
        tracing::warn!("could not save the learned app launches: {e}");
    }
    r
}

/// Everything learned so far, for the admin panel.
pub fn all_skills() -> Vec<Skill> {
    let mut v = state().lock().unwrap_or_else(|p| p.into_inner()).store.skills.clone();
    v.sort_by(|a, b| b.last_unix.cmp(&a.last_unix));
    v
}

pub fn forget(key: &str) -> bool {
    with_store(|s| s.forget(key))
}

pub fn forget_all() -> usize {
    with_store(|s| {
        let n = s.skills.len();
        s.skills.clear();
        n
    })
}

// ---- one launch of a name at a time ----

struct Slot {
    reply: Mutex<Option<String>>,
    done: Condvar,
}

struct Recent {
    key: String,
    at: Instant,
    stem: Option<String>,
    /// The window the launch opened (so "already open" is only said while it still is).
    hwnd: Option<isize>,
}

static INFLIGHT: Mutex<Vec<(String, Arc<Slot>)>> = Mutex::new(Vec::new());
static RECENT: Mutex<Vec<Recent>> = Mutex::new(Vec::new());

enum Turn {
    Lead(Arc<Slot>),
    Follow(Arc<Slot>),
    AlreadyOpen,
}

fn begin(key: &str) -> Turn {
    let mut inflight = INFLIGHT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, slot)) = inflight.iter().find(|(k, _)| k == key) {
        return Turn::Follow(slot.clone());
    }
    {
        let mut recent = RECENT.lock().unwrap_or_else(|p| p.into_inner());
        recent.retain(|r| r.at.elapsed() < DUPLICATE_WINDOW);
        if let Some(r) = recent.iter().find(|r| r.key == key) {
            let still_open = match r.hwnd {
                Some(h) => sys::visible_windows().iter().any(|w| w.hwnd == h),
                None => sys::visible_windows().iter().any(|w| is_match(w, key, r.stem.as_deref())),
            };
            if still_open {
                return Turn::AlreadyOpen;
            }
        }
    }
    let slot = Arc::new(Slot { reply: Mutex::new(None), done: Condvar::new() });
    inflight.push((key.to_string(), slot.clone()));
    Turn::Lead(slot)
}

fn finish(key: &str, slot: &Slot, reply: &str, opened: Option<(Option<String>, Option<isize>)>) {
    if let Some((stem, hwnd)) = opened {
        RECENT.lock().unwrap_or_else(|p| p.into_inner()).push(Recent { key: key.to_string(), at: Instant::now(), stem, hwnd });
    }
    INFLIGHT.lock().unwrap_or_else(|p| p.into_inner()).retain(|(k, _)| k != key);
    *slot.reply.lock().unwrap_or_else(|p| p.into_inner()) = Some(reply.to_string());
    slot.done.notify_all();
}

// ---- starting something and seeing it appear ----

/// Is this window the one a launch of `key` (a program called `stem`) opens?
fn is_match(w: &sys::Win, key: &str, stem: Option<&str>) -> bool {
    let exe = w.exe.strip_suffix(".exe").unwrap_or(&w.exe);
    if stem.is_some_and(|s| !s.is_empty() && exe == s) {
        return true;
    }
    let flat = |s: &str| -> String { s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect() };
    let k = flat(key);
    k.len() >= 3 && (flat(&w.title).contains(&k) || flat(exe).contains(&k))
}

enum Attempt {
    /// A new window of the program appeared (milliseconds, its handle, the program's name).
    Seen(u32, isize, Option<String>),
    /// Windows accepted the start, but no new window was seen in time.
    Started(Option<String>),
    /// The program was already open and its window was brought forward (it keeps a single window).
    AlreadyRunning(Option<String>),
    /// Nothing was started.
    Failed(String),
}

fn attempt(recipe: &Recipe, key: &str) -> Attempt {
    if let Err(why) = recipe_valid(recipe) {
        return Attempt::Failed(why);
    }
    if recipe.kind == Kind::Path && !std::path::Path::new(&recipe.target).exists() {
        return Attempt::Failed("the file is no longer there".into());
    }
    let stem = recipe.program_stem();
    let windows = sys::visible_windows();
    let before: Vec<isize> = windows.iter().map(|w| w.hwnd).collect();
    // Windows of this program that are open already: if no new one appears, the program probably kept its single window.
    let open_already: Vec<isize> = windows.iter().filter(|w| is_match(w, key, stem.as_deref())).map(|w| w.hwnd).collect();
    // Only a Start menu app or settings page (which usually keep one window) gets the short wait; a program that opens a window
    // of its own each time (Notepad) is given the full time, because a cold start can take two seconds or more.
    let quick = !open_already.is_empty() && matches!(recipe.kind, Kind::AppsFolder | Kind::Uri);
    let wait = if quick { SEE_WINDOW_IF_OPEN } else { SEE_WINDOW };
    let t0 = Instant::now();
    if let Err(e) = sys::shell_execute(&recipe.shell_target()) {
        return Attempt::Failed(e);
    }
    while t0.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(40));
        let now = sys::visible_windows();
        if let Some(w) = now.iter().find(|w| !before.contains(&w.hwnd) && is_match(w, key, stem.as_deref())) {
            return Attempt::Seen(t0.elapsed().as_millis() as u32, w.hwnd, stem);
        }
        // Nothing new, but the window that was open is still there and the program is answering: it was brought forward.
        if quick && t0.elapsed() > Duration::from_millis(500) && now.iter().any(|w| open_already.contains(&w.hwnd)) {
            return Attempt::AlreadyRunning(stem);
        }
    }
    if !open_already.is_empty() && sys::visible_windows().iter().any(|w| open_already.contains(&w.hwnd)) {
        return Attempt::AlreadyRunning(stem);
    }
    Attempt::Started(stem)
}

/// A program or shortcut the file index is sure about, as a recipe.
fn index_recipe(key: &str) -> Option<Recipe> {
    let ix = file_index()?;
    let res = ix.lookup(&LookupRequest { query: key.to_string(), want: Want::Program, limit: 3 });
    let top = res.found.first()?;
    let second = res.found.get(1).map_or(i32::MIN, |f| f.score);
    let clear = top.score >= 70 && (res.found.len() == 1 || top.score - second >= 25);
    if !clear || !matches!(top.kind, "executable" | "shortcut") {
        return None;
    }
    let recipe = Recipe { kind: Kind::Path, target: top.path.to_string_lossy().into_owned() };
    recipe_valid(&recipe).ok().map(|_| recipe)
}

fn started_reply(shown: &str, seen: bool) -> String {
    if seen {
        format!("Opened {shown}.")
    } else {
        format!("I started {shown}, but I haven't seen its window yet.")
    }
}

/// Reward or penalise what was tried, and learn a recipe that worked for the first time.
fn record(key: &str, shown: &str, recipe: &Recipe, source: &str, result: &Attempt, opt: &Options) {
    if !opt.learn {
        return;
    }
    let now = now_unix();
    let had = with_store(|s| s.get(key).is_some_and(|k| k.recipe == *recipe));
    match result {
        Attempt::Seen(ms, _, _) => {
            let first = !had;
            with_store(|s| {
                if first {
                    s.put(Skill::new(key, shown, recipe.clone(), source, now));
                }
                if let Some(k) = s.get_mut(key) {
                    k.reward(*ms, now);
                }
            });
            if first {
                emit("launch_skill_learned", json!({"app": shown, "source": source, "how": recipe.kind, "ms": ms}));
            }
        }
        Attempt::Started(_) => {
            with_store(|s| {
                if let Some(k) = s.get_mut(key).filter(|k| k.recipe == *recipe) {
                    k.note_unverified(now);
                }
            });
        }
        // The program was already open and Windows brought it forward: good evidence the recipe is right, so it is learned
        // (with no reward yet: nothing new was seen).
        Attempt::AlreadyRunning(_) => {
            let first = !had;
            with_store(|s| {
                if first {
                    s.put(Skill::new(key, shown, recipe.clone(), source, now));
                }
                if let Some(k) = s.get_mut(key) {
                    k.note_unverified(now);
                }
            });
            if first {
                emit("launch_skill_learned", json!({"app": shown, "source": source, "how": recipe.kind}));
            }
        }
        Attempt::Failed(why) => {
            let dropped = with_store(|s| {
                let Some(k) = s.get_mut(key).filter(|k| k.recipe == *recipe) else { return false };
                k.penalise(now);
                let drop = !k.usable();
                if drop {
                    s.forget(key);
                }
                drop
            });
            if had {
                emit("launch_skill_failed", json!({"app": shown, "why": why, "dropped": dropped}));
            }
        }
    }
}

static START_MENU: Mutex<Option<(Instant, Vec<(String, String)>)>> = Mutex::new(None);

/// The Start menu entry a spoken name means, as a recipe. Reads the Start menu once (one PowerShell, a second or two) and keeps it.
fn start_menu_recipe(key: &str) -> Option<Recipe> {
    let cached = START_MENU.lock().unwrap_or_else(|p| p.into_inner()).as_ref().filter(|(at, _)| at.elapsed() < START_MENU_KEPT).map(|(_, a)| a.clone());
    let apps = match cached {
        Some(a) => a,
        None => {
            let a = parse_start_apps(&sys::start_apps_csv(Duration::from_secs(10))?);
            if a.is_empty() {
                return None;
            }
            *START_MENU.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), a.clone()));
            a
        }
    };
    let (_, id) = best_start_app(key, &apps)?;
    let recipe = Recipe { kind: Kind::AppsFolder, target: id };
    recipe_valid(&recipe).ok().map(|_| recipe)
}

fn refusal(shown: &str, why: &str) -> String {
    if crate::windows_mcp::guard::shell_or_admin_reason(why) {
        format!("I can't open {shown}: it is a shell or administration tool, and I don't start those.")
    } else {
        format!("I can't open {shown}: {why}")
    }
}

/// Start `name`. `slow` is the Windows-MCP route, used only when nothing faster started the app. The returned text is what to say.
pub fn launch(name: &str, caller: Caller, opt: &Options, slow: &mut dyn FnMut(&str) -> Slow) -> String {
    let started = Instant::now();
    let reply = launch_inner(name, caller, opt, slow);
    // A launch goes round the tool host (it has its own guard), so it writes its own line in the host's ledger.
    let ok = reply.starts_with("Opened ") || reply.starts_with("I started ") || reply.contains("is already open") || reply.contains("is still starting");
    let who = match caller {
        Caller::Voice => "voice",
        Caller::Agent => "agent",
        Caller::Api => "api",
    };
    aqua_tool_host::record_direct(who, "app.launch", &display(&normalize(name)), ok, started.elapsed().as_millis() as u64);
    reply
}

fn launch_inner(name: &str, caller: Caller, opt: &Options, slow: &mut dyn FnMut(&str) -> Slow) -> String {
    let key = normalize(name);
    if key.is_empty() {
        return "Which app should I open?".into();
    }
    let shown = display(&key);
    if let Verdict::Deny(_, why) = evaluate("App", &json!({"mode": "launch", "name": name}), caller, None) {
        return refusal(&shown, &why);
    }
    match begin(&key) {
        Turn::AlreadyOpen => format!("{shown} is already open."),
        Turn::Follow(slot) => {
            let mut g = slot.reply.lock().unwrap_or_else(|p| p.into_inner());
            let deadline = Instant::now() + FOLLOW_WAIT;
            while g.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                g = slot.done.wait_timeout(g, left).unwrap_or_else(|p| p.into_inner()).0;
            }
            g.clone().unwrap_or_else(|| format!("{shown} is still starting."))
        }
        Turn::Lead(slot) => {
            let (reply, opened) = work(&key, &shown, opt, slow);
            finish(&key, &slot, &reply, opened);
            reply
        }
    }
}

type Opened = Option<(Option<String>, Option<isize>)>;

fn work(key: &str, shown: &str, opt: &Options, slow: &mut dyn FnMut(&str) -> Slow) -> (String, Opened) {
    let t0 = Instant::now();
    let mut tried: Vec<Recipe> = Vec::new();
    // In order, each only looked up when the one before did not start the app.
    for stage in 0..4 {
        let candidate: Option<(Recipe, &str)> = match stage {
            0 if opt.use_learned => with_store(|s| s.get(key).filter(|k| k.usable()).cloned()).map(|k| (k.recipe, "learned")),
            1 => well_known(key).map(|(_, r)| (r, "well-known")),
            2 if well_known(key).is_none() => index_recipe(key).map(|r| (r, "file index")),
            3 if well_known(key).is_none() => start_menu_recipe(key).map(|r| (r, "start menu")),
            _ => None,
        };
        let Some((recipe, source)) = candidate else { continue };
        if tried.contains(&recipe) {
            continue;
        }
        let result = attempt(&recipe, key);
        record(key, shown, &recipe, source, &result, opt);
        match result {
            Attempt::Seen(ms, hwnd, stem) => {
                tracing::info!("opened {shown} in {ms} ms ({source}: {:?} {})", recipe.kind, recipe.target);
                return (started_reply(shown, true), Some((stem, Some(hwnd))));
            }
            Attempt::AlreadyRunning(stem) => {
                tracing::info!("{shown} was already open; brought forward ({source})");
                return (format!("{shown} is already open."), Some((stem, None)));
            }
            // Windows accepted the start: never start anything else for this request.
            Attempt::Started(stem) => {
                tracing::info!("started {shown} ({source}), window not seen within {} ms", SEE_WINDOW.as_millis());
                return (started_reply(shown, false), Some((stem, None)));
            }
            Attempt::Failed(why) => {
                tracing::warn!("{source} way to open {shown} did not work: {why}");
                tried.push(recipe);
            }
        }
    }
    // Nothing faster worked: the Windows-MCP helper.
    match slow(shown) {
        Slow::Opened => {
            tracing::info!("opened {shown} through the Windows helper in {} ms", t0.elapsed().as_millis());
            (format!("Opened {shown}."), Some((None, None)))
        }
        Slow::Denied(sentence) => (sentence, None),
        Slow::Unknown => (format!("I asked Windows to open {shown}, but it didn't answer in time. It may still open."), Some((None, None))),
        // Nothing was started, so looking for a game, a folder or a program somewhere on the disk cannot make a second one.
        Slow::Failed => match crate::local_files::open_named(shown, Want::Any) {
            Ok(found) => (found, None),
            Err(_) => (format!("I couldn't open {shown}, and I didn't find anything with that name on this computer."), None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(title: &str, exe: &str) -> sys::Win {
        sys::Win { hwnd: 1, title: title.into(), exe: exe.into() }
    }

    #[test]
    fn a_new_window_is_recognised_by_its_program_or_its_title() {
        assert!(is_match(&win("Untitled - Notepad", "notepad.exe"), "notepad", Some("notepad")));
        assert!(is_match(&win("Calculator", "applicationframehost.exe"), "calculator", Some("calc")), "a Store app hosted by another program: by its title");
        assert!(is_match(&win("Spotify Premium", "spotify.exe"), "spotify", None));
        assert!(!is_match(&win("Inbox - Mail", "chrome.exe"), "notepad", Some("notepad")));
        assert!(!is_match(&win("x", "chrome.exe"), "x", None), "a name that short matches nothing by title");
    }

    #[test]
    fn a_second_request_for_a_name_that_is_starting_waits_and_shares_the_answer() {
        let key = "test-follow-app";
        let Turn::Lead(slot) = begin(key) else { panic!("the first request leads") };
        let Turn::Follow(follower) = begin(key) else { panic!("the second follows") };
        let waiter = std::thread::spawn(move || {
            let mut g = follower.reply.lock().unwrap();
            while g.is_none() {
                g = follower.done.wait(g).unwrap();
            }
            g.clone().unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        finish(key, &slot, "Opened Test.", None);
        assert_eq!(waiter.join().unwrap(), "Opened Test.");
        // The launch did not open a window we know of, so the name is free again.
        assert!(matches!(begin(key), Turn::Lead(_)));
        INFLIGHT.lock().unwrap().retain(|(k, _)| k != key);
    }

    #[test]
    fn the_same_name_while_another_launch_is_in_flight_is_never_led_twice() {
        let key = "test-single-flight";
        let leaders = (0..20)
            .map(|_| std::thread::spawn(move || matches!(begin(key), Turn::Lead(_))))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|l| *l)
            .count();
        assert_eq!(leaders, 1, "exactly one of twenty simultaneous requests starts the app");
        INFLIGHT.lock().unwrap().retain(|(k, _)| k != key);
    }

    #[test]
    fn shells_are_refused_before_anything_is_tried() {
        let mut called = false;
        let r = launch("PowerShell", Caller::Voice, &Options { learn: true, use_learned: true }, &mut |_| {
            called = true;
            Slow::Opened
        });
        assert!(r.contains("shell or administration tool"), "{r}");
        assert!(!called);
        assert!(launch("Command Prompt", Caller::Voice, &Options { learn: true, use_learned: true }, &mut |_| Slow::Opened).contains("can't open"));
    }

    #[test]
    fn an_empty_name_asks_which_app() {
        assert_eq!(launch("   ", Caller::Voice, &Options { learn: true, use_learned: true }, &mut |_| Slow::Opened), "Which app should I open?");
    }

    #[test]
    fn the_replies_say_what_happened() {
        assert_eq!(started_reply("Notepad", true), "Opened Notepad.");
        assert!(started_reply("Notepad", false).contains("haven't seen"));
    }
}
