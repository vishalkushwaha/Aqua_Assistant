//! Smart placement of the voice orb: when the window or page in front changes, look once at the new window and, only if
//! the orb is covering something, move it to the clearest nearby spot. Windows tells us when things change (no polling
//! while events work), a change is looked at once after a short quiet moment, and the orb never moves while it is being
//! dragged, while its text box is open, or when the person has locked it.
//!
//! The decisions (what is "busy", which spot is best, what is remembered) live in the `aqua-orb-placement` crate and are
//! tested there; this file wires them to Windows and to the admin panel.

mod sys;

use aqua_orb_placement::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

/// Size of the orb window (logical pixels) when nothing but the orb is showing.
const ORB_BOX: f32 = 300.0;
/// The sphere sits inside the window with transparent space around it; only the sphere (and the label under it) counts as "the orb".
const INSET_SIDE: f32 = 40.0;
const INSET_TOP: f32 = 40.0;
const INSET_BOTTOM: f32 = 8.0;
/// The gap kept between the orb and the screen edge.
const MARGIN: f32 = 16.0;

const QUIET_MS: u64 = 250;
const MAX_WAIT_MS: u64 = 1000;
const SETTLE_MS: u64 = 1500;
const SAVE_DELAY_MS: u64 = 600;
const POLL_MS: u64 = 1500;
/// How long the wheel must be still before the final look after a scroll.
const SCROLL_QUIET_MS: u64 = 400;
/// After the orb has moved because of scrolling, it is not moved again for scrolling for this long.
const SCROLL_MOVE_GAP_MS: u64 = 800;
/// After the person drags the orb, scrolling does not move it for this long.
const USER_PAUSE_MS: u64 = 6000;
const LOG_MAX: usize = 30;
const MOVE_MS: u64 = 220;
/// Where the orb starts when the person has never chosen a spot: the bottom-right corner.
const DEFAULT_SPOT: Spot = Spot { fx: 1.0, fy: 1.0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Foreground,
    Minimize,
    Restore,
    MoveSizeStart,
    MoveSizeEnd,
    Location,
    Name,
}

pub enum Msg {
    Event(EventKind, isize),
    /// The screens, their resolution or the taskbar changed.
    Display,
    Settings,
    Recheck,
    OrbShown,
    OrbResized,
    MoveTo(Spot),
    Reset,
    Poll,
    /// The mouse wheel was turned over this screen point.
    Wheel(i32, i32),
}

#[derive(Default, Clone, Copy, Serialize)]
struct Counters {
    events: u64,
    evaluations: u64,
    captures: u64,
    moves: u64,
    kept: u64,
    skipped: u64,
    /// Times the window was looked at because of the mouse wheel, and how many of those had to look at the whole window.
    scroll_checks: u64,
    scroll_wide: u64,
}

#[derive(Clone, Serialize)]
struct Entry {
    /// Seconds since 1970.
    t: u64,
    kind: &'static str,
    text: String,
}

#[derive(Default)]
struct Status {
    detection: String,
    app: String,
    monitor: String,
    fullscreen: bool,
    capture: String,
    capture_ms: u64,
    log: VecDeque<Entry>,
    counters: Counters,
    worker_cpu_ms: u64,
    note: Option<String>,
    /// The last time the orb was left alone without looking at a window, and why (so "it did nothing" can be explained).
    last_skip: Option<(u64, String)>,
}

struct Inner {
    saved: Mutex<Saved>,
    path: PathBuf,
    status: Mutex<Status>,
    tx: Sender<Msg>,
    orb: AtomicIsize,
    /// The person is typing in the orb's text box (the only time the orb is not moved because its window is taller).
    editing: AtomicBool,
    started: Instant,
}

/// Managed by Tauri; the commands below and the orb's own commands reach the service through it.
pub struct Placement(Arc<Inner>);

fn unix_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How the orb window maps to "the orb" (the part that matters), at this screen's scale.
#[derive(Clone, Copy)]
struct Geo {
    il: i32,
    it: i32,
    ib: i32,
    margin: i32,
    box_h: i32,
}

fn geo(orb: isize) -> Geo {
    let s = sys::dpi(orb) as f32 / 96.0;
    let px = |v: f32| (v * s).round() as i32;
    Geo { il: px(INSET_SIDE), it: px(INSET_TOP), ib: px(INSET_BOTTOM), margin: px(MARGIN), box_h: px(ORB_BOX) }
}

fn footprint(win: &Rect, g: &Geo) -> Rect {
    Rect::new(win.x + g.il, win.y + g.it, (win.w - 2 * g.il).max(40), (win.h - g.it - g.ib).max(40))
}

fn window_pos(fp: &Rect, g: &Geo) -> (i32, i32) {
    (fp.x - g.il, fp.y - g.it)
}

/// The screen holding most of `r` (or the nearest one).
fn monitor_for(mons: &[Monitor], r: &Rect) -> Option<Monitor> {
    mons.iter()
        .max_by_key(|m| r.intersect(&m.rect).map_or(-(((m.rect.center().0 - r.center().0).abs() + (m.rect.center().1 - r.center().1).abs()) as i64), |i| i.area()))
        .cloned()
}

fn is_shell_surface(w: &sys::WindowInfo) -> bool {
    const CLASSES: &[&str] = &[
        "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Progman", "WorkerW", "XamlExplorerHostIslandWindow", "TaskSwitcherWnd", "MultitaskingViewFrame",
        "Windows.UI.Core.CoreWindow", "NotifyIconOverflowWindow", "TopLevelWindowForOverflowXamlIsland", "ForegroundStaging", "ApplicationFrameInputSinkWindow",
    ];
    const EXES: &[&str] = &["searchhost.exe", "startmenuexperiencehost.exe", "shellexperiencehost.exe", "textinputhost.exe", "lockapp.exe", "logonui.exe"];
    CLASSES.contains(&w.class.as_str()) || EXES.contains(&w.exe.as_str())
}

impl Inner {
    fn log(&self, kind: &'static str, text: String) {
        let mut st = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let line = format!("{} {kind}: {text}\n", unix_secs());
        st.log.push_front(Entry { t: unix_secs(), kind, text });
        st.log.truncate(LOG_MAX);
        drop(st);
        self.append_log_file(&line);
    }

    /// Keep the same lines in `placement.log` next to the settings, so something that happened a while ago can still be read. It is
    /// started afresh (the old one kept as `.1`) when it passes 200 KB.
    fn append_log_file(&self, line: &str) {
        use std::io::Write;
        let path = self.path.with_file_name("placement.log");
        if std::fs::metadata(&path).map(|m| m.len() > 200 * 1024).unwrap_or(false) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn count(&self, f: impl FnOnce(&mut Counters)) {
        f(&mut self.status.lock().unwrap_or_else(|p| p.into_inner()).counters);
    }

    fn orb(&self) -> isize {
        self.orb.load(Ordering::Relaxed)
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn save(&self) {
        let r = self.saved.lock().unwrap_or_else(|p| p.into_inner()).save(&self.path);
        if let Err(e) = r {
            self.status.lock().unwrap_or_else(|p| p.into_inner()).note = Some(format!("could not save the orb positions: {e}"));
        }
    }

    fn state(&self) -> Value {
        let saved = self.saved.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let st = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let orb = self.orb();
        let win = sys::window_rect(orb);
        json!({
            "supported": cfg!(windows),
            "settings": saved.settings,
            "preferred": saved.preferred.map(|s| s.label()),
            "apps": saved.apps.iter().map(|a| json!({"app": a.app, "spot": a.spot.label()})).collect::<Vec<_>>(),
            "pages": saved.pages.len(),
            "status": {
                "detection": st.detection,
                "app": st.app,
                "fullscreen": st.fullscreen,
                "capture": st.capture,
                "capture_ms": st.capture_ms,
                "note": st.note,
                "last_skip": st.last_skip.as_ref().map(|(t, s)| json!({"t": t, "text": s})),
                "orb": win.map(|w| json!({"x": w.x, "y": w.y, "visible": sys::is_visible(orb)})),
            },
            "log": st.log,
            "counters": st.counters,
            "cpu_ms": {"worker": st.worker_cpu_ms, "hooks": sys::hook_cpu_ms()},
            "uptime_s": self.started.elapsed().as_secs(),
        })
    }

    /// Slide the orb window to a new place, easing out. Stops at once if the person grabs the orb.
    fn animate(&self, to: (i32, i32)) {
        let orb = self.orb();
        let Some(from) = sys::window_rect(orb) else { return };
        let t0 = Instant::now();
        loop {
            if sys::drag_in_progress() {
                return;
            }
            let p = (t0.elapsed().as_millis() as f32 / MOVE_MS as f32).min(1.0);
            let e = 1.0 - (1.0 - p).powi(3);
            let x = from.x + ((to.0 - from.x) as f32 * e).round() as i32;
            let y = from.y + ((to.1 - from.y) as f32 * e).round() as i32;
            sys::set_pos(orb, x, y);
            if p >= 1.0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

struct Worker {
    inner: Arc<Inner>,
    rx: Receiver<Msg>,
    deb: Debouncer,
    forced: Option<Why>,
    last_sig: Option<ContextSig>,
    /// The last ordinary window looked at, so a re-check asked for from Aqua's own panel still knows which window to look at.
    last_target: isize,
    window_moving: bool,
    follow_up_at: Option<u64>,
    save_at: Option<u64>,
    polling: bool,
    /// Mouse-wheel bursts: when to look, and where the wheel last turned.
    scroll: ScrollGate,
    scroll_pt: (i32, i32),
    /// Look at this window instead of the one in front (the one the wheel turned over).
    target_override: Option<isize>,
    /// Scrolling does not move the orb before this time (the person has just dragged it, or it has just moved).
    paused_until: u64,
    last_scroll_move: u64,
    /// The window and the background brightness found by the last look at the whole of it, for looks at one area only.
    last_bg: Option<(isize, u8)>,
    /// Where the orb was before it was pushed up to make room for its text box: (y before, y after).
    shifted: Option<(i32, i32)>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let now = self.inner.now();
            let wait = [self.deb.deadline(), self.follow_up_at, self.save_at, self.scroll.deadline()].into_iter().flatten().min();
            let got = match (wait, self.polling) {
                (Some(d), _) => self.rx.recv_timeout(Duration::from_millis(d.saturating_sub(now))),
                (None, true) => self.rx.recv_timeout(Duration::from_millis(POLL_MS)),
                (None, false) => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match got {
                Err(RecvTimeoutError::Disconnected) => break,
                Ok(m) => self.guarded(move |w| w.handle(m)),
                Err(RecvTimeoutError::Timeout) => {
                    if self.polling && !self.deb.pending() {
                        self.guarded(|w| w.handle(Msg::Poll));
                    }
                }
            }
            let now = self.inner.now();
            if self.deb.take_due(now) {
                let f = self.forced.take();
                self.guarded(move |w| w.evaluate(f, false));
            }
            if self.follow_up_at.is_some_and(|t| now >= t) {
                self.follow_up_at = None;
                self.guarded(|w| w.evaluate(Some(Why::Settled), true));
            }
            if self.scroll.take_due(now).is_some() {
                self.guarded(|w| w.after_scroll());
            }
            if self.save_at.is_some_and(|t| now >= t) {
                self.save_at = None;
                self.inner.save();
            }
            self.inner.status.lock().unwrap_or_else(|p| p.into_inner()).worker_cpu_ms = sys::thread_cpu_ms();
        }
    }

    /// A failure while looking at one window (or a bug) is written down and the service carries on.
    fn guarded(&mut self, f: impl FnOnce(&mut Worker)) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))).is_err() {
            self.inner.log("error", "something went wrong while looking at a window; the orb was left where it is".into());
        }
    }

    fn handle(&mut self, msg: Msg) {
        let now = self.inner.now();
        match msg {
            Msg::Event(kind, h) => {
                self.inner.count(|c| c.events += 1);
                if h == self.inner.orb() {
                    if kind == EventKind::MoveSizeEnd {
                        self.guarded(|w| w.user_moved());
                    }
                    return;
                }
                match kind {
                    EventKind::Foreground => {
                        if let Some(w) = sys::describe(h) {
                            if w.pid != std::process::id() {
                                sys::listen_to(w.pid, w.tid);
                            }
                        }
                        self.window_moving = false;
                        self.deb.push(now);
                    }
                    EventKind::Minimize | EventKind::Restore => self.deb.push(now),
                    EventKind::MoveSizeStart => self.window_moving = h == sys::foreground(),
                    EventKind::MoveSizeEnd => {
                        self.window_moving = false;
                        if h == sys::foreground() {
                            self.deb.push(now);
                        }
                    }
                    EventKind::Location => {
                        if !self.window_moving && h == sys::foreground() {
                            self.deb.push(now);
                        }
                    }
                    EventKind::Name => {
                        if h == sys::foreground() {
                            self.deb.push(now);
                        }
                    }
                }
            }
            Msg::Display => {
                self.forced = Some(Why::DisplayChanged);
                self.deb.push(now);
            }
            Msg::Wheel(x, y) => {
                self.scroll_pt = (x, y);
                self.scroll.event(now);
            }
            Msg::Settings => {
                self.apply_wheel_hook();
                self.forced = Some(Why::SettingsChanged);
                self.deb.push(now);
                self.save_at = Some(now + SAVE_DELAY_MS);
            }
            Msg::Recheck | Msg::OrbShown => {
                self.forced = Some(Why::Manual);
                self.deb.push(now);
            }
            Msg::OrbResized => self.guarded(|w| w.orb_resized()),
            Msg::MoveTo(spot) => self.guarded(|w| w.move_to(spot)),
            Msg::Reset => {
                self.inner.saved.lock().unwrap_or_else(|p| p.into_inner()).preferred = None;
                self.guarded(|w| w.move_to(DEFAULT_SPOT));
                self.inner.saved.lock().unwrap_or_else(|p| p.into_inner()).preferred = None;
                self.save_at = Some(now + SAVE_DELAY_MS);
            }
            Msg::Poll => self.deb.push(now),
        }
    }

    /// The wheel listener exists only while it is wanted: automatic moving on, not locked, and "move when I scroll" on.
    fn apply_wheel_hook(&mut self) {
        let s = self.inner.saved.lock().unwrap_or_else(|p| p.into_inner()).settings.clone();
        self.scroll.configure(u64::from(s.scroll_interval_ms), s.scroll_live);
        let want = s.may_auto_move() && s.scroll_move && !self.polling;
        sys::set_wheel_hook(want);
        if !want {
            self.scroll.cancel();
        }
    }

    /// A look is due after scrolling: at the window the wheel turned over, unless that is the orb or Aqua itself.
    fn after_scroll(&mut self) {
        if self.inner.now() < self.paused_until {
            return;
        }
        let (x, y) = self.scroll_pt;
        let h = sys::window_at(x, y);
        if h == 0 || h == self.inner.orb() {
            return;
        }
        self.inner.count(|c| c.scroll_checks += 1);
        self.target_override = Some(h);
        self.evaluate(Some(Why::Scrolled), false);
        self.target_override = None;
    }

    /// One picture of the window, analysed: all of it, or only the area `local` (then with the background found earlier).
    fn look(&mut self, info: &sys::WindowInfo, settings: &Settings, local: Option<Rect>) -> Option<BusyMap> {
        if !settings.avoid_content {
            return None;
        }
        let hint = if local.is_some() { self.last_bg.filter(|(h, _)| *h == info.hwnd).map(|(_, b)| b) } else { None };
        let t0 = Instant::now();
        let r = sys::capture(info.hwnd, info.rect, local, hint);
        let ms = t0.elapsed().as_millis() as u64;
        let mut st = self.inner.status.lock().unwrap_or_else(|p| p.into_inner());
        st.capture_ms = ms;
        match r {
            Ok(m) => {
                st.counters.captures += 1;
                st.capture = if m.stats.blank { "ok (the window looked blank)".into() } else if m.stats.local { "ok (area around the orb)".into() } else { "ok".into() };
                if local.is_none() {
                    self.last_bg = Some((info.hwnd, m.stats.background));
                }
                Some(m)
            }
            Err(e) => {
                st.capture = e;
                None
            }
        }
    }

    /// The person moved the orb by hand: that spot becomes their preferred one (and the program's / page's, if those are remembered).
    fn user_moved(&mut self) {
        let inner = self.inner.clone();
        let orb = inner.orb();
        let Some(win) = sys::window_rect(orb) else { return };
        let g = geo(orb);
        let fp = footprint(&win, &g);
        let mons = sys::monitors();
        let Some(mon) = monitor_for(&mons, &fp) else { return };
        let spot = Spot::from_rect(&fp, &usable_area(&mon.work, (fp.w, fp.h), g.margin));
        let mut text = format!("You moved the orb. Saved as your preferred spot ({})", spot.label());
        {
            let mut s = inner.saved.lock().unwrap_or_else(|p| p.into_inner());
            s.preferred = Some(spot);
            if let Some(sig) = &self.last_sig {
                if s.settings.per_app && !sig.app.is_empty() {
                    s.remember_app(&sig.app, spot, unix_ms());
                    text += &format!(" and for {}", sig.app);
                }
                if s.settings.per_page && !sig.app.is_empty() {
                    s.remember_page(&format!("{:016x}", sig.page), &sig.app, spot, unix_ms());
                    text += " and for this page";
                }
            }
        }
        inner.log("saved", text + ".");
        // Do not move the orb away again straight after the person put it somewhere.
        self.paused_until = inner.now() + USER_PAUSE_MS;
        self.scroll.cancel();
        self.save_at = Some(inner.now() + SAVE_DELAY_MS);
    }

    /// A move the person asked for in the panel ("Move the orb to: bottom-left").
    fn move_to(&mut self, spot: Spot) {
        let inner = self.inner.clone();
        let orb = inner.orb();
        let Some(win) = sys::window_rect(orb) else { return };
        let g = geo(orb);
        let mons = sys::monitors();
        // The primary screen for the default spot; otherwise the screen the orb is on.
        let mon = if spot == DEFAULT_SPOT && !mons.iter().any(|m| m.rect.intersect(&win).is_some()) { mons.iter().find(|m| m.primary).cloned() } else { monitor_for(&mons, &win) };
        let Some(mon) = mon else { return };
        let fp = footprint(&win, &g);
        let target = spot.to_rect((fp.w, fp.h), &usable_area(&mon.work, (fp.w, fp.h), g.margin));
        inner.animate(window_pos(&target, &g));
        inner.saved.lock().unwrap_or_else(|p| p.into_inner()).preferred = Some(spot);
        inner.log("saved", format!("You chose the {} for the orb.", spot.label()));
        self.save_at = Some(inner.now() + SAVE_DELAY_MS);
    }

    /// The orb window grew (text box or answer) or shrank back. Make sure a grown window is not hanging off the bottom of the screen.
    fn orb_resized(&mut self) {
        let orb = self.inner.orb();
        let Some(win) = sys::window_rect(orb) else { return };
        let g = geo(orb);
        let mons = sys::monitors();
        let Some(mon) = monitor_for(&mons, &win) else { return };
        if win.h > g.box_h + 4 {
            if win.bottom() > mon.work.bottom() {
                let y = (mon.work.bottom() - win.h).max(mon.work.y);
                sys::set_pos(orb, win.x, y);
                self.shifted = Some((win.y, y));
            }
        } else if let Some((before, after)) = self.shifted.take() {
            if (win.y - after).abs() <= 2 {
                sys::set_pos(orb, win.x, before);
            }
        }
        if win.h <= g.box_h + 4 {
            // Back to just the orb: a good moment to look at the window again.
            self.forced = Some(Why::Manual);
            self.deb.push(self.inner.now());
        }
    }

    fn skip(&self, force: Option<Why>, why: &str) {
        self.inner.count(|c| c.skipped += 1);
        self.inner.status.lock().unwrap_or_else(|p| p.into_inner()).last_skip = Some((unix_secs(), why.to_string()));
        if force.is_some_and(|w| w != Why::Settled && w != Why::Scrolled) {
            self.inner.log("skipped", why.to_string());
        }
    }

    fn evaluate(&mut self, force: Option<Why>, followup: bool) {
        let inner = self.inner.clone();
        let settings = inner.saved.lock().unwrap_or_else(|p| p.into_inner()).settings.clone();
        if !followup {
            inner.count(|c| c.evaluations += 1);
        }
        let orb = inner.orb();
        if orb == 0 {
            return self.skip(force, "the orb window was not found");
        }
        if !settings.may_auto_move() {
            return self.skip(force, if settings.lock { "the orb is locked, so it was left where it is" } else { "automatic moving is switched off" });
        }
        if sys::stale_drag_cleared() {
            self.user_moved();
        }
        if sys::drag_in_progress() {
            return self.skip(force, "you are dragging the orb");
        }
        if !sys::is_visible(orb) {
            return self.skip(force, "the orb is hidden");
        }
        let Some(win) = sys::window_rect(orb) else { return };
        let g = geo(orb);
        if inner.editing.load(Ordering::Relaxed) {
            return self.skip(force, "you are typing in the orb's text box, so it was left where it is");
        }

        // Which window to look at: the one in front, unless that is Aqua itself or part of Windows' own shell.
        let own = std::process::id();
        let fit = |w: &sys::WindowInfo| w.pid != own && !is_shell_surface(w) && !w.overlay && !w.iconic && !w.cloaked && w.visible && w.rect.w >= 200 && w.rect.h >= 150;
        let mut info = sys::describe(self.target_override.unwrap_or_else(sys::foreground)).filter(|w| fit(w));
        if info.is_none() && force.is_some() && force != Some(Why::Scrolled) && self.last_target != 0 {
            info = sys::describe(self.last_target).filter(|w| fit(w));
        }
        let Some(info) = info else { return self.skip(force, "no ordinary window is in front") };
        self.last_target = info.hwnd;

        let mons = sys::monitors();
        let Some(mon) = monitor_for(&mons, &info.rect) else { return };
        let fullscreen = !info.has_caption && info.rect.x <= mon.rect.x && info.rect.y <= mon.rect.y && info.rect.right() >= mon.rect.right() && info.rect.bottom() >= mon.rect.bottom();
        let salt = inner.saved.lock().unwrap_or_else(|p| p.into_inner()).salt;
        let sig = ContextSig { app: info.exe.clone(), hwnd: info.hwnd as i64, page: page_hash(salt, &info.exe, &info.title), rect: info.rect, monitor: mon.id.clone(), fullscreen };
        let why = force.or_else(|| change(self.last_sig.as_ref(), &sig));
        let Some(why) = why else { return };
        self.last_sig = Some(sig.clone());
        {
            let mut st = inner.status.lock().unwrap_or_else(|p| p.into_inner());
            st.app = sig.app.clone();
            st.monitor = mon.id.clone();
            st.fullscreen = fullscreen;
        }
        if !followup && matches!(why, Why::AppSwitched | Why::PageChanged | Why::MonitorChanged) {
            // Pages often finish painting after their title changes: look once more shortly.
            self.follow_up_at = Some(inner.now() + SETTLE_MS);
        }

        let fp = footprint(&win, &g);
        let size = (fp.w, fp.h);
        let usable = usable_area(&mon.work, size, g.margin);
        let page_key = format!("{:016x}", sig.page);
        let homes = {
            let sv = inner.saved.lock().unwrap_or_else(|p| p.into_inner());
            Homes {
                page: if settings.per_page { sv.page_spot(&page_key).map(|s| s.to_rect(size, &usable)) } else { None },
                app: if settings.per_app && !sig.app.is_empty() { sv.app_spot(&sig.app).map(|s| s.to_rect(size, &usable)) } else { None },
                preferred: sv.preferred.map(|s| s.to_rect(size, &usable)),
                default: Some(DEFAULT_SPOT.to_rect(size, &usable)),
            }
        };
        let scrolled = why == Why::Scrolled;
        // After a scroll only the area around the orb is analysed at first: its own size, and three quarters of it again on
        // every side, which is as far as the nearby spots reach. If nothing nearby is clear, the whole window is looked at.
        let local = (scrolled && settings.avoid_content && !fullscreen).then(|| fp.inflate(fp.w * 3 / 4).intersect(&mon.work).unwrap_or(fp));
        let cursor = sys::cursor();
        let mut busy = self.look(&info, &settings, local);
        let mut d = decide(&Input { work: mon.work, size, margin: g.margin, current: fp, busy: busy.as_ref(), cursor, homes, fullscreen, restrict: local, settings: &settings });
        if local.is_some() && busy.is_some() && !d.moves && d.obstructed {
            inner.count(|c| c.scroll_wide += 1);
            busy = self.look(&info, &settings, None);
            d = decide(&Input { work: mon.work, size, margin: g.margin, current: fp, busy: busy.as_ref(), cursor, homes, fullscreen, restrict: None, settings: &settings });
        }
        let app = if sig.app.is_empty() { "a program".to_string() } else { sig.app.clone() };
        // Do not move the orb again for scrolling straight after it moved for scrolling.
        let hold = scrolled && d.moves && inner.now() < self.last_scroll_move + SCROLL_MOVE_GAP_MS;
        if d.moves && !hold {
            if scrolled {
                self.last_scroll_move = inner.now();
            }
            inner.count(|c| c.moves += 1);
            inner.log("moved", format!("{app} ({}): moved to the {}: {}.", why.text(), Spot::from_rect(&d.to, &usable).label(), d.reason));
            inner.animate(window_pos(&d.to, &g));
        } else {
            inner.count(|c| c.kept += 1);
            if why != Why::Settled && why != Why::Scrolled {
                inner.log("kept", format!("{app} ({}): {}.", why.text(), d.reason));
            }
        }
    }
}

impl Placement {
    /// Load what was remembered, put the orb where the person last had it, and start listening for window changes.
    pub fn start(app: &AppHandle) -> Placement {
        let path = crate::data_dir().map(|d| d.join("orb")).unwrap_or_else(|| std::env::temp_dir().join("aqua-orb")).join("placement.json");
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
        let (saved, note) = Saved::load(&path, seed);
        let (tx, rx) = channel::<Msg>();
        let inner = Arc::new(Inner { saved: Mutex::new(saved), path, status: Mutex::new(Status { note, ..Status::default() }), tx: tx.clone(), orb: AtomicIsize::new(0), editing: AtomicBool::new(false), started: Instant::now() });

        #[cfg(windows)]
        if let Some(h) = app.get_webview_window("voice-overlay").and_then(|w| w.hwnd().ok()) {
            let h = h.0 as isize;
            inner.orb.store(h, Ordering::Relaxed);
            sys::ORB.store(h, Ordering::Relaxed);
        }
        #[cfg(not(windows))]
        let _ = app;

        place_at_start(&inner);

        let hooks = if std::env::var_os("AQUA_ORB_NO_HOOKS").is_some() { Err("switched off with AQUA_ORB_NO_HOOKS".to_string()) } else { sys::start_hooks(tx) };
        let polling = match hooks {
            Ok(()) => {
                inner.status.lock().unwrap_or_else(|p| p.into_inner()).detection = "Windows window events (no polling)".into();
                false
            }
            Err(e) => {
                inner.status.lock().unwrap_or_else(|p| p.into_inner()).detection = format!("looking at the front window every {} seconds (window events were not available: {e})", POLL_MS as f32 / 1000.0);
                true
            }
        };
        let worker = Worker { inner: inner.clone(), rx, deb: Debouncer::new(QUIET_MS, MAX_WAIT_MS), forced: None, last_sig: None, last_target: 0, window_moving: false, follow_up_at: None, save_at: None, polling, scroll: ScrollGate::new(350, SCROLL_QUIET_MS, true), scroll_pt: (0, 0), target_override: None, paused_until: 0, last_scroll_move: 0, last_bg: None, shifted: None };
        let mut worker = worker;
        worker.apply_wheel_hook();
        let _ = std::thread::Builder::new().name("aqua-orb-placement".into()).spawn(move || worker.run());
        Placement(inner)
    }

    fn send(&self, m: Msg) {
        let _ = self.0.tx.send(m);
    }

    /// The orb window changed size (its text box opened or closed).
    pub fn orb_resized(&self, editing: bool) {
        self.0.editing.store(editing, Ordering::Relaxed);
        self.send(Msg::OrbResized);
    }

    /// The orb was brought back (tray icon, "Task in Text"): the window in front may have changed while it was hidden.
    pub fn orb_shown(&self) {
        self.send(Msg::OrbShown);
    }
}

/// At start the orb goes to the spot the person last chose, or the bottom-right corner of the main screen the first time.
fn place_at_start(inner: &Inner) {
    let orb = inner.orb();
    let Some(win) = sys::window_rect(orb) else { return };
    let mons = sys::monitors();
    let Some(mon) = mons.iter().find(|m| m.primary).or(mons.first()) else { return };
    let g = geo(orb);
    let fp = footprint(&win, &g);
    let spot = inner.saved.lock().unwrap_or_else(|p| p.into_inner()).preferred.unwrap_or(DEFAULT_SPOT);
    let target = spot.to_rect((fp.w, fp.h), &usable_area(&mon.work, (fp.w, fp.h), g.margin));
    let (x, y) = window_pos(&target, &g);
    sys::set_pos(orb, x, y);
}

fn spot_named(name: &str) -> Option<Spot> {
    let (fy, fx) = match name {
        "top-left" => (0.0, 0.0),
        "top-center" => (0.0, 0.5),
        "top-right" => (0.0, 1.0),
        "middle-left" => (0.5, 0.0),
        "middle-right" => (0.5, 1.0),
        "bottom-left" => (1.0, 0.0),
        "bottom-center" => (1.0, 0.5),
        "bottom-right" => (1.0, 1.0),
        _ => return None,
    };
    Some(Spot { fx, fy })
}

/// Everything the panel shows: settings, what is remembered, what was decided lately, and what it costs.
#[tauri::command]
pub fn orb_place_state(p: State<'_, Placement>) -> Value {
    p.0.state()
}

/// Change settings: an object with any of auto_move, lock, avoid_content, avoid_cursor, per_app, per_page, sensitivity.
#[tauri::command]
pub fn orb_place_set(p: State<'_, Placement>, patch: Value) -> Result<Value, String> {
    let changed = p.0.saved.lock().unwrap_or_else(|e| e.into_inner()).settings.apply_patch(&patch)?;
    if !changed.is_empty() {
        p.0.log("setting", format!("Changed: {}.", changed.join(", ").replace('_', " ")));
        p.send(Msg::Settings);
    }
    Ok(p.0.state())
}

/// Move the orb to a named spot ("top-left" ... "bottom-right", or "default") and keep it there as the preferred spot.
#[tauri::command]
pub fn orb_place_move(p: State<'_, Placement>, spot: String) -> Result<(), String> {
    if spot == "default" {
        p.send(Msg::Reset);
        return Ok(());
    }
    let s = spot_named(&spot).ok_or("unknown spot")?;
    p.send(Msg::MoveTo(s));
    Ok(())
}

/// Forget what was remembered for one program (and its pages), or everything if no program is named.
#[tauri::command]
pub fn orb_place_forget(p: State<'_, Placement>, app: Option<String>) -> Result<Value, String> {
    {
        let mut s = p.0.saved.lock().unwrap_or_else(|e| e.into_inner());
        match &app {
            Some(a) => {
                s.forget_app(a);
            }
            None => s.forget_all(),
        }
    }
    p.0.log("forgot", match &app {
        Some(a) => format!("Forgot the spots remembered for {a}."),
        None => "Forgot every remembered spot.".into(),
    });
    p.0.save();
    Ok(p.0.state())
}

/// One part of the orb window that takes mouse clicks (CSS pixels, relative to the window).
#[derive(serde::Deserialize)]
pub struct HitShape {
    kind: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// The orb's window is a transparent square, most of it empty. This clips it to the sphere (a circle) plus the label and answer
/// panels, so a click anywhere else goes to the file or button underneath instead of waking the orb.
#[tauri::command]
pub fn orb_hit_regions(window: tauri::WebviewWindow, shapes: Vec<HitShape>, dpr: f64) -> Result<(), String> {
    if window.label() != "voice-overlay" {
        return Err("only the orb window can be shaped".into());
    }
    if shapes.len() > 16 || !(0.5..=4.0).contains(&dpr) || shapes.iter().any(|s| ![s.x, s.y, s.w, s.h].iter().all(|v| v.is_finite() && v.abs() < 5000.0) || s.w <= 0.0 || s.h <= 0.0) {
        return Err("the shapes are not valid".into());
    }
    #[cfg(windows)]
    {
        let hwnd = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
        let list: Vec<(bool, f64, f64, f64, f64)> = shapes.iter().map(|s| (s.kind == "ellipse", s.x, s.y, s.w, s.h)).collect();
        window.run_on_main_thread(move || sys::set_window_region(hwnd, &list, dpr)).map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    let _ = (window, shapes, dpr);
    Ok(())
}

/// Look at the window in front again now.
#[tauri::command]
pub fn orb_place_recheck(p: State<'_, Placement>) {
    p.send(Msg::Recheck);
}
