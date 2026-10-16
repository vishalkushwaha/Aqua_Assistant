//! The threads. One owns the engine (clock, ticks, saving); one owns the Windows source (notifications, listings, battery
//! reads). A failure in either is caught and reported; nothing here can stop the rest of Aqua.

use super::engine::{Action, DeliveryContext, Engine, Snapshot, BASELINE_MS};
use super::model::*;
use super::notify::{Allow, Announcement};
use super::settings::*;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

enum EngineMsg {
    Raw(RawEvent),
    Stop,
}

#[derive(Debug)]
enum SourceMsg {
    /// Windows says something changed (or the engine wants a fresh look, for example after waking).
    Trigger,
    /// Read this device's battery now (confirms a low reading).
    Reread { id: String, after: Duration },
    Stop,
}

#[derive(Default)]
struct Stats {
    engine_cpu_ms: AtomicU64,
    source_cpu_ms: AtomicU64,
}

pub struct Monitor {
    engine: Arc<Mutex<Engine>>,
    start: Instant,
    tx: Sender<EngineMsg>,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    source_tx: Mutex<Option<Sender<SourceMsg>>>,
}

static GLOBAL: OnceLock<Arc<Monitor>> = OnceLock::new();

/// Start the monitor once for this process (saved settings live in `dir`).
pub fn init_global(dir: PathBuf) -> Result<()> {
    if GLOBAL.get().is_some() {
        return Ok(());
    }
    let m = Monitor::start(dir)?;
    let _ = GLOBAL.set(m);
    Ok(())
}

pub fn global() -> Option<&'static Arc<Monitor>> {
    GLOBAL.get()
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_salt() -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let local = 0u8;
    let addr = &local as *const u8 as u64;
    let mut h = nanos ^ addr.rotate_left(17) ^ (std::process::id() as u64).rotate_left(41);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h | 1
}

/// CPU time used by the calling thread, in milliseconds (0 where unavailable).
#[cfg(windows)]
fn thread_cpu_ms() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    // SAFETY: plain data filled by the call.
    unsafe {
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = std::mem::zeroed();
        if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) == 0 {
            return 0;
        }
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        (ft(k) + ft(u)) / 10_000
    }
}
#[cfg(not(windows))]
fn thread_cpu_ms() -> u64 {
    0
}

#[cfg(windows)]
fn local_hour() -> Option<u8> {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: SYSTEMTIME is plain data filled by the call.
    unsafe {
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        Some(t.wHour as u8)
    }
}
#[cfg(not(windows))]
fn local_hour() -> Option<u8> {
    None
}

#[cfg(windows)]
fn quiet_now() -> bool {
    super::source::win::quiet_reason().is_some()
}
#[cfg(not(windows))]
fn quiet_now() -> bool {
    false
}

impl Monitor {
    /// Start monitoring this computer. `dir` holds the saved settings and known devices.
    pub fn start(dir: PathBuf) -> Result<Arc<Monitor>> {
        Self::start_with(dir, cfg!(windows))
    }

    /// `with_source = false` starts only the engine: events are then injected (tests and the simulator).
    pub fn start_with(dir: PathBuf, with_source: bool) -> Result<Arc<Monitor>> {
        std::fs::create_dir_all(&dir)?;
        let state_path = dir.join("state.json");
        let (mut state, warning) = State::load(&state_path);
        if state.salt == 0 {
            state.salt = random_salt();
            let _ = state.save(&state_path);
        }
        state.prune(unix_secs());
        let mut engine = Engine::new(state.settings.clone(), state.known.clone(), state.salt, unix_secs());
        engine.warning = warning;
        engine.begin_baseline(0, BASELINE_MS);
        let engine = Arc::new(Mutex::new(engine));
        let (tx, rx) = channel::<EngineMsg>();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let monitor = Arc::new(Monitor { engine: engine.clone(), start: Instant::now(), tx: tx.clone(), stop: stop.clone(), stats: stats.clone(), source_tx: Mutex::new(None) });

        if with_source {
            let (stx, srx) = channel::<SourceMsg>();
            *monitor.source_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(stx.clone());
            let (etx, sstats, sstop) = (tx.clone(), stats.clone(), stop.clone());
            std::thread::Builder::new().name("aqua-device-source".into()).spawn(move || source_supervisor(srx, stx, etx, sstats, sstop))?;
        }

        let m = monitor.clone();
        std::thread::Builder::new().name("aqua-device-engine".into()).spawn(move || engine_loop(m, rx, state_path, state))?;
        Ok(monitor)
    }

    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = self.now_ms();
        self.lock().snapshot(now)
    }

    pub fn version(&self) -> u64 {
        self.lock().version()
    }

    pub fn pending(&self) -> usize {
        self.lock().pending_count()
    }

    /// CPU seconds used by the two monitor threads since start.
    pub fn cpu_seconds(&self) -> (f64, f64) {
        (self.stats.engine_cpu_ms.load(Ordering::Relaxed) as f64 / 1000.0, self.stats.source_cpu_ms.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// The next announcement the player may speak, or None.
    pub fn next(&self, allow: Allow) -> Option<Announcement> {
        let ctx = DeliveryContext { quiet: quiet_now(), local_hour: local_hour() };
        let now = self.now_ms();
        self.lock().next_for_speaker(now, allow, ctx)
    }

    pub fn ack(&self, id: u64, spoken: bool) {
        let now = self.now_ms();
        self.lock().ack(now, id, spoken);
    }

    pub fn set_settings(&self, s: Settings) -> Result<(), String> {
        let now = self.now_ms();
        self.lock().set_settings(s, now)
    }

    pub fn set_device_prefs(&self, reference: &str, prefs: DevicePrefs) -> Result<(), String> {
        let now = self.now_ms();
        self.lock().set_device_prefs(reference, prefs, now)
    }

    pub fn forget(&self, reference: &str) -> bool {
        let now = self.now_ms();
        self.lock().forget(reference, now)
    }

    pub fn forget_all(&self) {
        let now = self.now_ms();
        self.lock().forget_all(now);
    }

    pub fn test_announcement(&self) -> u64 {
        let now = self.now_ms();
        self.lock().test_announcement(now)
    }

    /// Feed an event as if a source had produced it (tests, and the simulator used to check the whole chain).
    pub fn inject(&self, ev: RawEvent) {
        let _ = self.tx.send(EngineMsg::Raw(ev));
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(EngineMsg::Stop);
        if let Some(s) = self.source_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = s.send(SourceMsg::Stop);
        }
    }

    fn ask_source(&self, msg: SourceMsg) {
        if let Some(s) = self.source_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = s.send(msg);
        }
    }
}

fn engine_loop(m: Arc<Monitor>, rx: Receiver<EngineMsg>, state_path: PathBuf, mut state: State) {
    let mut last_loop = SystemTime::now();
    let mut last_save = Instant::now();
    let mut save_due = false;
    loop {
        let msg = rx.recv_timeout(Duration::from_millis(500));
        // Woke much later than planned: the computer slept. Devices re-connect for a while; do not announce them as new.
        let gap = SystemTime::now().duration_since(last_loop).unwrap_or_default();
        last_loop = SystemTime::now();
        let now = m.now_ms();
        let mut actions: Vec<Action> = Vec::new();
        {
            let mut e = m.lock();
            if gap > Duration::from_secs(120) {
                e.note_resume(now);
                drop(e);
                m.ask_source(SourceMsg::Trigger);
                e = m.lock();
            }
            match msg {
                Ok(EngineMsg::Raw(ev)) => actions.extend(e.ingest(now, ev)),
                Ok(EngineMsg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // drain a burst
            while let Ok(EngineMsg::Raw(ev)) = rx.try_recv() {
                actions.extend(e.ingest(now, ev));
            }
            actions.extend(e.tick(now));
            if e.take_dirty() {
                save_due = true;
                state.settings = e.settings().clone();
                state.known = e.known().clone();
            }
        }
        for a in actions {
            match a {
                Action::Reread { id, after_ms } => m.ask_source(SourceMsg::Reread { id, after: Duration::from_millis(after_ms) }),
            }
        }
        if save_due && last_save.elapsed() > Duration::from_secs(2) {
            state.prune(unix_secs());
            let _ = state.save(&state_path);
            save_due = false;
            last_save = Instant::now();
        }
        m.stats.engine_cpu_ms.store(thread_cpu_ms(), Ordering::Relaxed);
        if m.stop.load(Ordering::SeqCst) {
            break;
        }
    }
    if save_due {
        let _ = state.save(&state_path);
    }
}

fn source_supervisor(rx: Receiver<SourceMsg>, tx: Sender<SourceMsg>, engine: Sender<EngineMsg>, stats: Arc<Stats>, stop: Arc<AtomicBool>) {
    let mut crashes: Vec<Instant> = Vec::new();
    let rx = Mutex::new(rx);
    loop {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
            run_source(&rx, &tx, &engine, &stats, &stop)
        }));
        match r {
            Ok(()) => return,
            Err(_) => {
                crashes.retain(|t| t.elapsed() < Duration::from_secs(3600));
                crashes.push(Instant::now());
                let why = "the device source stopped unexpectedly".to_string();
                if crashes.len() >= 3 {
                    let _ = engine.send(EngineMsg::Raw(RawEvent::Source { name: "devices".into(), health: SourceHealth::Failed(format!("{why}; stopped after repeated failures")) }));
                    return;
                }
                let _ = engine.send(EngineMsg::Raw(RawEvent::Source { name: "devices".into(), health: SourceHealth::Failed(format!("{why}; restarting")) }));
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

#[cfg(not(windows))]
fn run_source(_rx: &Receiver<SourceMsg>, _tx: &Sender<SourceMsg>, engine: &Sender<EngineMsg>, _stats: &Stats, _stop: &AtomicBool) {
    let _ = engine.send(EngineMsg::Raw(RawEvent::Source { name: "devices".into(), health: SourceHealth::Unavailable("device monitoring is only available on Windows".into()) }));
}

#[cfg(windows)]
fn run_source(rx: &Receiver<SourceMsg>, tx: &Sender<SourceMsg>, engine: &Sender<EngineMsg>, stats: &Stats, stop: &AtomicBool) {
    use super::source::win::*;
    use std::collections::HashMap;

    let send = |ev: RawEvent| {
        let _ = engine.send(EngineMsg::Raw(ev));
    };
    // The callbacks signal through this channel (the same one the engine's re-read requests use).
    let (wake_tx, wake_rx) = channel::<()>();
    let (regs, _flag, errors) = Registrations::new(wake_tx);
    let polling_only = !regs.started;
    if polling_only {
        send(RawEvent::Source { name: "devices".into(), health: SourceHealth::Unavailable(format!("{}; checking every 15 seconds instead", errors.join("; "))) });
    } else {
        send(RawEvent::Source { name: "devices".into(), health: SourceHealth::Ok });
        for e in &errors {
            send(RawEvent::Source { name: "devices".into(), health: SourceHealth::Unavailable(format!("{e}; the other notification still works")) });
        }
    }

    let mut en = Enumerator::default();
    let mut last: HashMap<String, super::source::Group> = HashMap::new();
    let mut last_battery: HashMap<String, Option<u8>> = HashMap::new();
    let mut rereads: Vec<(String, Instant)> = Vec::new();
    let mut last_enum = Instant::now() - Duration::from_secs(60);
    let mut next_reconcile = Instant::now() + Duration::from_secs(600);
    let mut next_poll = Instant::now() + Duration::from_secs(300);
    let mut next_system = Instant::now() + Duration::from_secs(60);
    let mut computer_present = false;
    let mut want_enum = true;

    // The first listing, and this computer's own battery.
    let enumerate = |en: &mut Enumerator, last: &mut HashMap<String, super::source::Group>, last_battery: &mut HashMap<String, Option<u8>>, send: &dyn Fn(RawEvent)| {
        let found = en.devices();
        let mut now_ids = std::collections::HashSet::new();
        for g in found {
            now_ids.insert(g.info.id.clone());
            let changed = last.get(&g.info.id).map(|old| old.info != g.info).unwrap_or(true);
            if changed {
                let battery = g.battery_node.as_deref().and_then(bluetooth_battery);
                last_battery.insert(g.info.id.clone(), battery.and_then(|b| b.percent));
                send(RawEvent::Present { info: g.info.clone(), battery });
                last.insert(g.info.id.clone(), g);
            }
        }
        let gone: Vec<String> = last.keys().filter(|k| !now_ids.contains(*k)).cloned().collect();
        for id in gone {
            last.remove(&id);
            last_battery.remove(&id);
            send(RawEvent::Gone { id });
        }
    };

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();
        let mut deadline = next_reconcile.min(next_poll).min(next_system);
        if polling_only {
            deadline = deadline.min(last_enum + Duration::from_secs(15));
        }
        if let Some(due) = rereads.iter().map(|(_, t)| *t).min() {
            deadline = deadline.min(due);
        }
        if want_enum {
            deadline = now;
        }
        let wait = deadline.saturating_duration_since(now).min(Duration::from_secs(30));

        // Wait for: a message from the engine (re-read, trigger, stop), or a device change from Windows.
        match rx.recv_timeout(wait.min(Duration::from_millis(250))) {
            Ok(SourceMsg::Stop) => break,
            Ok(SourceMsg::Trigger) => want_enum = true,
            Ok(SourceMsg::Reread { id, after }) => rereads.push((id, Instant::now() + after)),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
        }
        if wake_rx.try_recv().is_ok() {
            want_enum = true;
            while wake_rx.try_recv().is_ok() {}
        }

        let now = Instant::now();
        if want_enum || now >= next_reconcile || (polling_only && now >= last_enum + Duration::from_secs(15)) {
            // collect a burst: a connection fires many notifications within a moment
            if want_enum && now.saturating_duration_since(last_enum) < Duration::from_millis(1000) {
                std::thread::sleep(Duration::from_millis(400));
            }
            enumerate(&mut en, &mut last, &mut last_battery, &send);
            regs.rearm();
            last_enum = Instant::now();
            next_reconcile = last_enum + Duration::from_secs(600);
            want_enum = false;
            // this computer's battery is a device too (once, if the machine has a battery)
            if !computer_present {
                if let Some(b) = system_battery() {
                    computer_present = true;
                    send(RawEvent::Present { info: computer_info(), battery: Some(b) });
                }
            }
        }

        // due re-reads (confirming a low battery)
        let now = Instant::now();
        let due: Vec<String> = rereads.iter().filter(|(_, t)| *t <= now).map(|(id, _)| id.clone()).collect();
        rereads.retain(|(_, t)| *t > now);
        for id in due {
            let reading = if id == "system:battery" { system_battery() } else { last.get(&id).and_then(|g| g.battery_node.as_deref()).and_then(bluetooth_battery) };
            if let Some(reading) = reading {
                send(RawEvent::Battery { id, reading });
            }
        }

        // the periodic battery read: slow normally, faster once something is low
        if now >= next_poll {
            let mut any_low = false;
            for (id, g) in &last {
                if let Some(node) = g.battery_node.as_deref() {
                    if let Some(reading) = bluetooth_battery(node) {
                        any_low |= reading.percent.is_some_and(|p| p <= 25);
                        // only changes are interesting, but an unchanged low value still matters to the engine's confirmation
                        let prev = last_battery.insert(id.clone(), reading.percent);
                        if prev != Some(reading.percent) || reading.percent.is_some_and(|p| p <= 25) {
                            send(RawEvent::Battery { id: id.clone(), reading });
                        }
                    }
                }
            }
            next_poll = Instant::now() + Duration::from_secs(if any_low { 60 } else { 300 });
        }
        if now >= next_system {
            let mut quick = false;
            if computer_present {
                if let Some(b) = system_battery() {
                    quick = b.percent.is_some_and(|p| p <= 25);
                    send(RawEvent::Battery { id: "system:battery".into(), reading: b });
                }
            }
            next_system = Instant::now() + Duration::from_secs(if quick { 30 } else { 60 });
        }
        stats.source_cpu_ms.store(thread_cpu_ms(), Ordering::Relaxed);
        let _ = tx; // kept so the channel stays open for the engine's messages
    }
    drop(regs);
}
