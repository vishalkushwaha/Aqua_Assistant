//! The decision maker. Pure: it never reads a clock (the caller passes `now`, in milliseconds), never touches Windows and
//! never speaks. Sources feed it `RawEvent`s; it keeps the registry, applies the suppression rules and settings, and puts
//! announcements into the queue. All of it is tested with a scripted clock.

use super::battery::*;
use super::model::*;
use super::notify::*;
use super::rules::{capitalise, display_name, has_or_have};
use super::settings::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use tracing::info;

/// After the first event of a device, wait this long for its other nodes before deciding.
pub const SETTLE_MS: u64 = 1_500;
/// A change must stay unchanged this long before it is announced: a device that flaps is announced once, in its final state.
pub const QUIET_MS: u64 = 5_000;
/// After start, devices that are already there are recorded silently.
pub const BASELINE_MS: u64 = 20_000;
/// After waking from sleep, devices that were connected before are not announced as new.
pub const RESUME_GRACE_MS: u64 = 180_000;
const ZERO_PLACEHOLDER_MS: u64 = 10_000;
const DEAD_AGE_MS: u64 = 10 * 60_000;
const TTL_SHORT_MS: u64 = 20_000;
const TTL_BATTERY_MS: u64 = 5 * 60_000;
const TTL_CRITICAL_MS: u64 = 60 * 60_000;
const LOG_KEEP: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Read this device's battery again after this long (confirms a low reading).
    Reread { id: String, after_ms: u64 },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "result", content = "why")]
pub enum Outcome {
    Announced(String),
    Suppressed(String),
    Info(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub at_ms: u64,
    pub device: String,
    pub kind: Option<EventKind>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counters {
    pub events: u64,
    pub announced: u64,
    pub suppressed: u64,
    pub expired: u64,
}

struct Dev {
    info: DeviceInfo,
    /// What the system says now.
    present: bool,
    /// What the user was last told (or silently assumed at the baseline).
    announced_present: bool,
    pending: bool,
    first_at: u64,
    last_at: u64,
    flips: u32,
    connected_at: u64,
    battery: Option<BatteryReading>,
    battery_at: u64,
    machine: BatteryMachine,
    first_battery_after_connect: bool,
    last_critical_at: Option<u64>,
}

/// What the player tells the core about the moment of delivery.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeliveryContext {
    /// A full-screen app, a presentation, Focus Assist or a call: only high priority items speak.
    pub quiet: bool,
    pub local_hour: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    /// An opaque reference for the API (a salted hash of the id: no addresses or serial numbers leave the core).
    #[serde(rename = "ref")]
    pub reference: String,
    pub name: String,
    pub device_type: DeviceType,
    pub type_text: String,
    pub connection: Connection,
    pub id_kind: IdKind,
    pub connected: bool,
    pub battery_percent: Option<u8>,
    pub charging: Charging,
    pub battery_source: BatterySource,
    /// ok | low | very_low | critical | not_reported | unknown
    pub battery_state: String,
    pub last_seen: u64,
    pub prefs: DevicePrefs,
    /// Another device has the same name: the panel shows a short suffix.
    pub same_name_as_another: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub version: u64,
    /// The engine's clock (milliseconds since it started); decision times are on the same clock.
    pub now_ms: u64,
    pub settings: Settings,
    pub devices: Vec<DeviceView>,
    pub pending: usize,
    pub sources: BTreeMap<String, SourceHealth>,
    pub recent: Vec<Decision>,
    pub queue: Vec<Announcement>,
    pub counters: Counters,
    pub warning: Option<String>,
}

pub struct Engine {
    settings: Settings,
    devices: BTreeMap<String, Dev>,
    known: BTreeMap<String, Known>,
    queue: Queue,
    log: VecDeque<Decision>,
    baseline_until: u64,
    resume_until: u64,
    resume_set: Vec<String>,
    sent: VecDeque<u64>,
    battery_seen: HashMap<(String, Level), u64>,
    charging_seen: HashMap<String, u64>,
    sources: BTreeMap<String, SourceHealth>,
    counters: Counters,
    version: u64,
    dirty: bool,
    salt: u64,
    wall_at_zero: u64,
    rereads: BTreeMap<String, u64>,
    pub warning: Option<String>,
}

fn fnv(salt: u64, s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325 ^ salt;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Engine {
    pub fn new(settings: Settings, known: BTreeMap<String, Known>, salt: u64, wall_secs_at_zero: u64) -> Engine {
        Engine {
            settings,
            devices: BTreeMap::new(),
            known,
            queue: Queue::default(),
            log: VecDeque::new(),
            baseline_until: 0,
            resume_until: 0,
            resume_set: Vec::new(),
            sent: VecDeque::new(),
            battery_seen: HashMap::new(),
            charging_seen: HashMap::new(),
            sources: BTreeMap::new(),
            counters: Counters::default(),
            version: 1,
            dirty: false,
            salt,
            wall_at_zero: wall_secs_at_zero,
            rereads: BTreeMap::new(),
            warning: None,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn pending_count(&self) -> usize {
        self.queue.pending_count()
    }
    pub fn known(&self) -> &BTreeMap<String, Known> {
        &self.known
    }
    /// True once since the last call if something worth saving changed.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn thresholds(&self) -> Thresholds {
        Thresholds { low: self.settings.low_percent, very_low: self.settings.very_low_percent, critical: self.settings.critical_percent }
    }

    pub fn device_ref(&self, id: &str) -> String {
        format!("{:012x}", fnv(self.salt, id) & 0xffff_ffff_ffff)
    }

    fn bump(&mut self) {
        self.version += 1;
    }

    fn record(&mut self, now: u64, id: &str, kind: Option<EventKind>, outcome: Outcome) {
        match &outcome {
            Outcome::Announced(_) => self.counters.announced += 1,
            Outcome::Suppressed(_) => self.counters.suppressed += 1,
            Outcome::Info(_) => {}
        }
        let label = if id.is_empty() { String::new() } else { self.devices.get(id).map(|d| display_name(&d.info)).unwrap_or_default() };
        info!(target: "aqua::device", "[DEVICE] {:?} device={} ({}) {:?}", kind, if id.is_empty() { "-".into() } else { self.device_ref(id) }, label, outcome);
        if self.log.len() >= LOG_KEEP {
            self.log.pop_front();
        }
        self.log.push_back(Decision { at_ms: now, device: label, kind, outcome });
        self.bump();
    }

    // ------------------------------------------------------------------------------------------------ start, resume

    /// Devices that are present in the next `window_ms` are recorded silently (they were there before Aqua started).
    pub fn begin_baseline(&mut self, now: u64, window_ms: u64) {
        self.baseline_until = now + window_ms;
    }

    /// The computer woke from sleep: devices that were connected before are not announced as new for a while.
    pub fn note_resume(&mut self, now: u64) {
        self.resume_until = now + RESUME_GRACE_MS;
        self.resume_set = self.devices.iter().filter(|(_, d)| d.announced_present).map(|(id, _)| id.clone()).collect();
        self.record(now, "", None, Outcome::Info("resumed from sleep: devices that were connected before are not announced as new".into()));
    }

    // ------------------------------------------------------------------------------------------------ events in

    pub fn ingest(&mut self, now: u64, ev: RawEvent) -> Vec<Action> {
        self.counters.events += 1;
        match ev {
            RawEvent::Present { info, battery } => {
                self.on_present(now, info, battery);
                Vec::new()
            }
            RawEvent::Gone { id } => {
                self.on_gone(now, &id);
                Vec::new()
            }
            RawEvent::Battery { id, reading } => self.on_battery(now, &id, reading, false),
            RawEvent::Source { name, health } => {
                if self.sources.get(&name) != Some(&health) {
                    if let SourceHealth::Failed(why) | SourceHealth::Unavailable(why) = &health {
                        self.record(now, "", Some(EventKind::SourceFailed), Outcome::Info(format!("{name}: {why}")));
                    }
                    self.sources.insert(name, health);
                    self.bump();
                }
                Vec::new()
            }
        }
    }

    fn remember(&mut self, now: u64, info: &DeviceInfo) {
        let k = Known { name: info.name.clone(), device_type: info.device_type, connection: info.connection, id_kind: info.id_kind, last_seen: self.wall_at_zero + now / 1000 };
        if self.known.get(&info.id) != Some(&k) {
            self.known.insert(info.id.clone(), k);
            self.dirty = true;
        }
    }

    fn on_present(&mut self, now: u64, info: DeviceInfo, battery: Option<BatteryReading>) {
        // Some headsets report 0 for "not known yet" right after connecting.
        let battery = battery.map(|mut b| {
            if b.percent == Some(0) {
                b.percent = None;
            }
            b
        });
        self.remember(now, &info);
        let id = info.id.clone();
        let in_baseline = now < self.baseline_until;
        let resumed = now < self.resume_until && self.resume_set.contains(&id);
        match self.devices.get_mut(&id) {
            None => {
                let silent = in_baseline || resumed || info.device_type == DeviceType::Computer;
                let dev = Dev {
                    info,
                    present: true,
                    announced_present: silent,
                    pending: !silent,
                    first_at: now,
                    last_at: now,
                    flips: 0,
                    connected_at: now,
                    battery,
                    battery_at: now,
                    machine: BatteryMachine::default(),
                    first_battery_after_connect: true,
                    last_critical_at: None,
                };
                self.devices.insert(id.clone(), dev);
                if silent {
                    let why = if resumed { "was connected before the computer slept" } else if in_baseline { "was already connected when Aqua started" } else { "this computer" };
                    self.record(now, &id, Some(EventKind::DeviceConnected), Outcome::Suppressed(why.into()));
                    // its battery is judged as a first reading
                    if let Some(b) = battery {
                        self.on_battery(now, &id, b, true);
                    }
                } else {
                    self.bump();
                }
            }
            Some(dev) => {
                dev.info = info;
                if let Some(b) = battery {
                    dev.battery = Some(b);
                    dev.battery_at = now;
                }
                if !dev.present {
                    dev.present = true;
                    dev.flips += 1;
                    if resumed || in_baseline {
                        dev.announced_present = true;
                        dev.pending = false;
                        dev.connected_at = now;
                    } else if dev.announced_present != dev.present {
                        if !dev.pending {
                            dev.first_at = now;
                        }
                        dev.pending = true;
                    }
                    dev.last_at = now;
                    dev.connected_at = now;
                    self.bump();
                }
            }
        }
    }

    fn on_gone(&mut self, now: u64, id: &str) {
        let in_baseline = now < self.baseline_until;
        let Some(dev) = self.devices.get_mut(id) else { return };
        if !dev.present {
            return;
        }
        dev.present = false;
        dev.flips += 1;
        dev.last_at = now;
        self.rereads.remove(id);
        if in_baseline {
            dev.announced_present = false;
            dev.pending = false;
        } else if dev.announced_present {
            if !dev.pending {
                dev.first_at = now;
            }
            dev.pending = true;
        } else {
            // it was never announced as connected: nothing to say now
            dev.pending = false;
        }
        self.bump();
    }

    fn on_battery(&mut self, now: u64, id: &str, reading: BatteryReading, immediate: bool) -> Vec<Action> {
        let Some(dev) = self.devices.get_mut(id) else { return Vec::new() };
        if !dev.present {
            return Vec::new(); // a stale value after a disconnect says nothing
        }
        // Some headsets report 0 for "not known yet" right after connecting.
        let mut reading = reading;
        if reading.percent == Some(0) && dev.first_battery_after_connect && now.saturating_sub(dev.connected_at) < ZERO_PLACEHOLDER_MS {
            reading.percent = None;
        }
        dev.battery = Some(reading);
        dev.battery_at = now;
        let announced = dev.announced_present;
        let first = dev.first_battery_after_connect;
        self.bump();
        if !announced {
            return Vec::new(); // judged when the connection is announced (merged into that sentence)
        }
        self.process_battery(now, id, reading, immediate || first, None)
    }

    // ------------------------------------------------------------------------------------------------ battery decisions

    /// Returns the actions asked for. `merge`: the connect announcement is being built; a warning is folded into it.
    fn process_battery(&mut self, now: u64, id: &str, reading: BatteryReading, immediate: bool, mut merge: Option<&mut Option<(Level, u8)>>) -> Vec<Action> {
        let th = self.thresholds();
        let infer = self.settings.infer_charging;
        let Some(dev) = self.devices.get_mut(id) else { return Vec::new() };
        if reading.percent.is_some() {
            dev.first_battery_after_connect = false;
        }
        let outcomes = dev.machine.reading(&reading, now, &th, infer, immediate);
        let mut actions = Vec::new();
        for o in outcomes {
            match o {
                Outcome2::Reread { after_ms } => {
                    self.rereads.insert(id.to_string(), now + after_ms);
                    actions.push(Action::Reread { id: id.to_string(), after_ms });
                }
                Outcome2::Warn { level, percent } => {
                    if let Some(slot) = merge.as_deref_mut() {
                        *slot = Some((level, percent));
                    } else {
                        self.emit_battery(now, id, level, percent);
                    }
                }
                Outcome2::Recovered { percent } => self.emit_simple(now, id, EventKind::BatteryRecovered, "recovered", Some(percent), Priority::Low),
                Outcome2::ChargingStarted => self.emit_charging(now, id, EventKind::DeviceCharging, "charging"),
                Outcome2::FullyCharged => self.emit_charging(now, id, EventKind::DeviceFullyCharged, "fully_charged"),
            }
        }
        actions
    }

    fn allow(&self, id: &str, kind: EventKind) -> Result<(), &'static str> {
        let s = &self.settings;
        if !s.enabled {
            return Err("device monitoring is switched off");
        }
        let prefs = s.prefs(id);
        if prefs.ignore {
            return Err("this device is ignored");
        }
        let Some(dev) = self.devices.get(id) else { return Err("unknown device") };
        if s.disabled_categories.contains(&dev.info.device_type.category()) {
            return Err("this kind of device is switched off");
        }
        let (over, general, label) = match kind {
            EventKind::DeviceConnected => (prefs.connect, s.announce_connections, "connection announcements are off"),
            EventKind::DeviceDisconnected => (prefs.disconnect, s.announce_disconnections, "disconnection announcements are off"),
            EventKind::BatteryLow | EventKind::BatteryVeryLow | EventKind::BatteryChanged => (prefs.battery, s.announce_low_battery, "low battery announcements are off"),
            EventKind::BatteryCritical => (prefs.battery, s.announce_critical_battery, "critical battery announcements are off"),
            EventKind::BatteryRecovered => (prefs.battery, s.announce_recovered, "battery recovery announcements are off"),
            EventKind::DeviceCharging => (prefs.charging, s.announce_charging, "charging announcements are off"),
            EventKind::DeviceFullyCharged => (prefs.charging, s.announce_fully_charged, "fully charged announcements are off"),
            EventKind::SourceFailed => (None, true, ""),
        };
        if over.unwrap_or(general) {
            if kind == EventKind::DeviceConnected && dev.info.device_type == DeviceType::UsbUnknown && over.is_none() && !s.announce_unknown_usb {
                return Err("announcements about unknown USB devices are off");
            }
            Ok(())
        } else if over.is_some() {
            Err("switched off for this device")
        } else {
            Err(label)
        }
    }

    fn render(&self, key: &str, info: &DeviceInfo, pct: Option<u8>) -> String {
        let name = display_name(info);
        let plural = has_or_have(&name) == "have";
        self.settings
            .template(key)
            .replace("{name}", &name)
            .replace("{has}", if plural { "have" } else { "has" })
            .replace("{is}", if plural { "are" } else { "is" })
            .replace("{pct}", &pct.map(|p| p.to_string()).unwrap_or_default())
    }

    fn rate_limited(&mut self, now: u64, prio: Priority) -> bool {
        while self.sent.front().is_some_and(|t| now.saturating_sub(*t) > 60_000) {
            self.sent.pop_front();
        }
        prio != Priority::High && self.sent.len() as u32 >= self.settings.rate_limit_per_minute
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue(&mut self, now: u64, id: &str, kind: EventKind, text: String, prio: Priority, ttl: u64, level: Option<Level>) {
        if self.rate_limited(now, prio) {
            self.record(now, id, Some(kind), Outcome::Suppressed("too many announcements in the last minute".into()));
            return;
        }
        let name = self.devices.get(id).map(|d| display_name(&d.info)).unwrap_or_default();
        self.sent.push_back(now);
        self.queue.push(Announcement { id: 0, priority: prio, kind, device_ids: vec![id.to_string()], names: vec![name], text: text.clone(), created_ms: now, expires_ms: now + ttl, state: AnnState::Pending, delivered_ms: 0, attempts: 0, level });
        self.record(now, id, Some(kind), Outcome::Announced(text));
    }

    fn is_computer(&self, id: &str) -> bool {
        self.devices.get(id).is_some_and(|d| d.info.device_type == DeviceType::Computer)
    }

    fn emit_battery(&mut self, now: u64, id: &str, level: Level, percent: u8) {
        let kind = match level {
            Level::Critical => EventKind::BatteryCritical,
            Level::VeryLow => EventKind::BatteryVeryLow,
            _ => EventKind::BatteryLow,
        };
        if let Err(why) = self.allow(id, kind) {
            self.record(now, id, Some(kind), Outcome::Suppressed(why.into()));
            return;
        }
        // The same level is not announced again within the cooldown, except critical.
        if level != Level::Critical {
            if let Some(t) = self.battery_seen.get(&(id.to_string(), level)) {
                if now.saturating_sub(*t) < self.settings.battery_cooldown_secs * 1000 {
                    self.record(now, id, Some(kind), Outcome::Suppressed("the same warning was given a short while ago".into()));
                    return;
                }
            }
        }
        self.battery_seen.insert((id.to_string(), level), now);
        let comp = self.is_computer(id);
        let key = match (level, comp) {
            (Level::Critical, false) => "battery_critical",
            (Level::Critical, true) => "computer_battery_critical",
            (_, false) => "battery_low",
            (_, true) => "computer_battery_low",
        };
        let info = self.devices[id].info.clone();
        let text = self.render(key, &info, Some(percent));
        let (prio, ttl) = if level == Level::Critical { (Priority::High, TTL_CRITICAL_MS) } else { (Priority::Medium, TTL_BATTERY_MS) };
        if level == Level::Critical {
            if let Some(d) = self.devices.get_mut(id) {
                d.last_critical_at = Some(now);
            }
        }
        self.enqueue(now, id, kind, text, prio, ttl, Some(level));
    }

    fn emit_simple(&mut self, now: u64, id: &str, kind: EventKind, key: &str, pct: Option<u8>, prio: Priority) {
        if let Err(why) = self.allow(id, kind) {
            self.record(now, id, Some(kind), Outcome::Suppressed(why.into()));
            return;
        }
        let info = self.devices[id].info.clone();
        let text = self.render(key, &info, pct);
        self.enqueue(now, id, kind, text, prio, TTL_SHORT_MS, None);
    }

    fn emit_charging(&mut self, now: u64, id: &str, kind: EventKind, key: &str) {
        if let Err(why) = self.allow(id, kind) {
            self.record(now, id, Some(kind), Outcome::Suppressed(why.into()));
            return;
        }
        if let Some(t) = self.charging_seen.get(id) {
            if now.saturating_sub(*t) < self.settings.charging_cooldown_secs * 1000 {
                self.record(now, id, Some(kind), Outcome::Suppressed("charging was announced a moment ago".into()));
                return;
            }
        }
        self.charging_seen.insert(id.to_string(), now);
        let key = if self.is_computer(id) { format!("computer_{key}") } else { key.to_string() };
        let info = self.devices[id].info.clone();
        let text = self.render(&key, &info, None);
        self.enqueue(now, id, kind, text, Priority::Low, TTL_SHORT_MS, None);
    }

    // ------------------------------------------------------------------------------------------------ time

    /// Decide what has settled, ask for due battery re-reads, repeat a critical warning if the user asked for that.
    pub fn tick(&mut self, now: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut ready: Vec<(u64, String)> = self.devices.iter().filter(|(_, d)| d.pending && now >= (d.first_at + SETTLE_MS).max(d.last_at + QUIET_MS)).map(|(id, d)| (d.first_at, id.clone())).collect();
        ready.sort();
        for (_, id) in ready {
            actions.extend(self.resolve_connection(now, &id));
        }
        // due re-reads that no later reading has satisfied
        let due: Vec<String> = self.rereads.iter().filter(|(_, t)| now >= **t).map(|(id, _)| id.clone()).collect();
        for id in due {
            self.rereads.remove(&id);
            actions.push(Action::Reread { id, after_ms: 0 });
        }
        // optional repeat of a critical warning
        let minutes = self.settings.repeat_critical_minutes as u64;
        if minutes > 0 {
            let th = self.thresholds();
            let again: Vec<(String, u8)> = self
                .devices
                .iter()
                .filter(|(_, d)| d.present && d.announced_present && d.machine.warned() == Level::Critical && d.machine.last_charging() != Charging::Charging)
                .filter(|(_, d)| d.last_critical_at.is_some_and(|t| now.saturating_sub(t) >= minutes * 60_000))
                .filter_map(|(id, d)| d.machine.last_percent().filter(|p| th.level_of(*p) == Level::Critical).map(|p| (id.clone(), p)))
                .collect();
            for (id, p) in again {
                self.emit_battery(now, &id, Level::Critical, p);
            }
        }
        for (a, why) in self.queue.expire(now) {
            if a.state == AnnState::Expired || a.state == AnnState::Dropped {
                self.counters.expired += 1;
            }
            let dev = a.device_ids.first().cloned().unwrap_or_default();
            self.record(now, &dev, Some(a.kind), Outcome::Suppressed(why.into()));
        }
        actions
    }

    fn resolve_connection(&mut self, now: u64, id: &str) -> Vec<Action> {
        let Some(dev) = self.devices.get_mut(id) else { return Vec::new() };
        dev.pending = false;
        let flips = std::mem::take(&mut dev.flips);
        if dev.present == dev.announced_present {
            if flips > 0 {
                self.record(now, id, None, Outcome::Suppressed("it changed and changed back, so there is nothing to say".into()));
            }
            return Vec::new();
        }
        if dev.present {
            dev.announced_present = true;
            dev.machine.reset();
            dev.first_battery_after_connect = true;
            dev.connected_at = now;
            let battery = dev.battery;
            self.announce_connect(now, id, battery)
        } else {
            dev.announced_present = false;
            let last = dev.machine.last_percent();
            let age = now.saturating_sub(dev.battery_at);
            dev.machine.reset();
            dev.battery = None;
            self.announce_disconnect(now, id, last, age);
            Vec::new()
        }
    }

    fn announce_connect(&mut self, now: u64, id: &str, battery: Option<BatteryReading>) -> Vec<Action> {
        // Judge the battery first: a warning is folded into the connect sentence.
        let mut merged: Option<(Level, u8)> = None;
        let mut actions = Vec::new();
        let allowed = self.allow(id, EventKind::DeviceConnected);
        if let Some(b) = battery.filter(|b| b.percent.is_some()) {
            let slot = if allowed.is_ok() { Some(&mut merged) } else { None };
            actions = self.process_battery(now, id, b, true, slot);
        }
        let info = self.devices[id].info.clone();
        match allowed {
            Err(why) => {
                self.record(now, id, Some(EventKind::DeviceConnected), Outcome::Suppressed(why.into()));
            }
            Ok(()) => {
                let (key, prio, level, pct) = match merged {
                    Some((Level::Critical, p)) => ("connected_battery_critical", Priority::High, Some(Level::Critical), Some(p)),
                    Some((l, p)) => ("connected_battery", Priority::Medium, Some(l), Some(p)),
                    None => ("connected", Priority::Medium, None, None),
                };
                // Several connections close together become one sentence.
                if merged.is_none() {
                    let name = display_name(&info);
                    let plain = self.queue.mergeable_connect(now).map(|a| a.level.is_none() && a.priority == Priority::Medium).unwrap_or(false);
                    if plain {
                        let many_tpl = self.settings.template("connected_many");
                        if let Some(a) = self.queue.mergeable_connect(now) {
                            a.names.push(name);
                            a.device_ids.push(id.to_string());
                            a.text = many_tpl.replace("{names}", &join_names(&a.names));
                            let text = a.text.clone();
                            self.record(now, id, Some(EventKind::DeviceConnected), Outcome::Announced(format!("joined with the previous connection: {text}")));
                            return actions;
                        }
                    }
                }
                let text = self.render(key, &info, pct);
                self.enqueue(now, id, EventKind::DeviceConnected, text, prio, TTL_SHORT_MS, level);
            }
        }
        actions
    }

    fn announce_disconnect(&mut self, now: u64, id: &str, last_percent: Option<u8>, battery_age_ms: u64) {
        let th = self.thresholds();
        let computer = self.is_computer(id);
        let dying = !computer && last_percent.is_some_and(|p| th.level_of(p) >= Level::VeryLow) || (!computer && last_percent.is_some_and(|p| th.level_of(p) >= Level::Low) && battery_age_ms > DEAD_AGE_MS);
        let critical = last_percent.is_some_and(|p| th.level_of(p) == Level::Critical);
        let info = self.devices[id].info.clone();
        if dying {
            // "It may have run out of battery": spoken if disconnections or battery warnings are wanted for this device.
            let ok = self.allow(id, EventKind::DeviceDisconnected).is_ok() || self.allow(id, EventKind::BatteryCritical).is_ok();
            if ok {
                let text = self.render("disconnected_dead", &info, last_percent);
                let (prio, ttl) = if critical { (Priority::High, TTL_BATTERY_MS) } else { (Priority::Medium, TTL_SHORT_MS) };
                self.enqueue(now, id, EventKind::DeviceDisconnected, text, prio, ttl, None);
                return;
            }
        }
        match self.allow(id, EventKind::DeviceDisconnected) {
            Err(why) => self.record(now, id, Some(EventKind::DeviceDisconnected), Outcome::Suppressed(why.into())),
            Ok(()) => {
                let text = self.render("disconnected", &info, None);
                self.enqueue(now, id, EventKind::DeviceDisconnected, text, Priority::Low, TTL_SHORT_MS, None);
            }
        }
    }

    // ------------------------------------------------------------------------------------------------ out

    /// The next item the player may speak now, or None. Items that are no longer true are dropped on the way.
    pub fn next_for_speaker(&mut self, now: u64, allow: Allow, ctx: DeliveryContext) -> Option<Announcement> {
        for (a, why) in self.queue.expire(now) {
            let dev = a.device_ids.first().cloned().unwrap_or_default();
            self.counters.expired += 1;
            self.record(now, &dev, Some(a.kind), Outcome::Suppressed(why.into()));
        }
        let quiet_hours = self.settings.quiet_hours.enabled
            && ctx.local_hour.is_some_and(|h| {
                let (s, e) = (self.settings.quiet_hours.start_hour, self.settings.quiet_hours.end_hour);
                if s <= e { h >= s && h < e } else { h >= s || h < e }
            });
        for id in self.queue.pending_ids() {
            let Some(a) = self.queue.get(id).cloned() else { continue };
            if allow == Allow::CriticalOnly && a.priority != Priority::High {
                continue;
            }
            if (ctx.quiet || quiet_hours) && a.priority != Priority::High {
                continue; // held (the item expires on its own if the quiet lasts)
            }
            if let Some(why) = self.invalid_now(&a) {
                self.queue.set_state(id, AnnState::Dropped, now);
                let dev = a.device_ids.first().cloned().unwrap_or_default();
                self.record(now, &dev, Some(a.kind), Outcome::Suppressed(why.into()));
                continue;
            }
            self.queue.set_state(id, AnnState::Delivered, now);
            return self.queue.get(id).cloned();
        }
        None
    }

    /// Is this item still true? (A queued "20 percent" must not be spoken after the battery has risen.)
    fn invalid_now(&self, a: &Announcement) -> Option<&'static str> {
        let id = a.device_ids.first()?;
        let dev = self.devices.get(id)?;
        if !self.settings.enabled || self.settings.prefs(id).ignore {
            return Some("switched off before it was spoken");
        }
        match a.kind {
            EventKind::DeviceConnected => (!dev.present).then_some("the device is no longer connected"),
            EventKind::BatteryLow | EventKind::BatteryVeryLow | EventKind::BatteryCritical => {
                if !dev.present {
                    return Some("the device is no longer connected");
                }
                let th = self.thresholds();
                let Some(level) = a.level else { return None };
                match dev.battery.and_then(|b| b.percent) {
                    Some(p) if th.level_of(p) < level && p >= th.threshold_of(level).saturating_add(RESET_MARGIN) => Some("the battery has risen since"),
                    _ if dev.battery.is_some_and(|b| b.charging == Charging::Charging) => Some("it is charging now"),
                    _ => None,
                }
            }
            EventKind::DeviceCharging => (dev.battery.map(|b| b.charging) != Some(Charging::Charging)).then_some("it is no longer charging"),
            _ => None,
        }
    }

    /// The player says what happened to an item it was given. A critical item that could not be spoken (the speech engine was
    /// busy or not ready) goes back to the queue and is tried again, up to five times; anything else is dropped.
    pub fn ack(&mut self, now: u64, id: u64, spoken: bool) {
        if let Some(a) = self.queue.get(id).cloned() {
            let dev = a.device_ids.first().cloned().unwrap_or_default();
            if spoken {
                self.queue.set_state(id, AnnState::Spoken, now);
                self.record(now, &dev, Some(a.kind), Outcome::Info("spoken".into()));
            } else if a.priority == Priority::High && a.attempts < 4 {
                self.queue.retry(id);
                self.record(now, &dev, Some(a.kind), Outcome::Info("could not be spoken just now: it will be tried again".into()));
            } else {
                self.queue.set_state(id, AnnState::Dropped, now);
                self.record(now, &dev, Some(a.kind), Outcome::Info("could not be spoken".into()));
            }
        }
    }

    pub fn test_announcement(&mut self, now: u64) -> u64 {
        let text = "This is a test of Aqua's device announcements.".to_string();
        let id = self.queue.push(Announcement { id: 0, priority: Priority::Medium, kind: EventKind::DeviceConnected, device_ids: vec![], names: vec![], text: text.clone(), created_ms: now, expires_ms: now + TTL_SHORT_MS, state: AnnState::Pending, delivered_ms: 0, attempts: 0, level: None });
        self.record(now, "", None, Outcome::Announced(text));
        id
    }

    // ------------------------------------------------------------------------------------------------ settings

    pub fn set_settings(&mut self, mut s: Settings, now: u64) -> Result<(), String> {
        s.validate()?;
        // A device taken off the ignore list does not announce what happened while it was ignored.
        for (id, old) in &self.settings.devices {
            let still = s.devices.get(id).map(|p| p.ignore).unwrap_or(false);
            if old.ignore && !still {
                if let Some(d) = self.devices.get_mut(id) {
                    d.announced_present = d.present;
                    d.pending = false;
                }
            }
        }
        // Devices are changed one at a time through `set_device_prefs`; a general update keeps them.
        if s.devices.is_empty() && !self.settings.devices.is_empty() {
            s.devices = self.settings.devices.clone();
        }
        self.settings = s;
        self.dirty = true;
        self.record(now, "", None, Outcome::Info("settings changed".into()));
        Ok(())
    }

    fn resolve_ref(&self, reference: &str) -> Option<String> {
        self.devices.keys().chain(self.known.keys()).find(|id| self.device_ref(id) == reference).cloned()
    }

    pub fn set_device_prefs(&mut self, reference: &str, prefs: DevicePrefs, now: u64) -> Result<(), String> {
        let id = self.resolve_ref(reference).ok_or("unknown device")?;
        let was_ignored = self.settings.prefs(&id).ignore;
        if prefs == DevicePrefs::default() {
            self.settings.devices.remove(&id);
        } else {
            self.settings.devices.insert(id.clone(), prefs.clone());
        }
        if was_ignored && !prefs.ignore {
            if let Some(d) = self.devices.get_mut(&id) {
                d.announced_present = d.present;
                d.pending = false;
            }
        }
        self.dirty = true;
        self.record(now, &id, None, Outcome::Info("device settings changed".into()));
        Ok(())
    }

    /// Remove a device's record and settings.
    pub fn forget(&mut self, reference: &str, now: u64) -> bool {
        let Some(id) = self.resolve_ref(reference) else { return false };
        let present = self.devices.get(&id).is_some_and(|d| d.present);
        self.known.remove(&id);
        self.settings.devices.remove(&id);
        if !present {
            self.devices.remove(&id);
        }
        self.dirty = true;
        self.record(now, "", None, Outcome::Info("a device was forgotten".into()));
        true
    }

    pub fn forget_all(&mut self, now: u64) {
        self.known.clear();
        self.settings.devices.clear();
        self.devices.retain(|_, d| d.present);
        self.dirty = true;
        self.record(now, "", None, Outcome::Info("all remembered devices were forgotten".into()));
    }

    // ------------------------------------------------------------------------------------------------ views

    pub fn snapshot(&self, now: u64) -> Snapshot {
        let th = self.thresholds();
        let mut views: Vec<DeviceView> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (id, d) in &self.devices {
            seen.insert(id.clone());
            let b = d.battery;
            let state = match b.and_then(|b| b.percent) {
                Some(p) => match th.level_of(p) {
                    Level::Ok => "ok",
                    Level::Low => "low",
                    Level::VeryLow => "very_low",
                    Level::Critical => "critical",
                },
                None if d.present => "not_reported",
                None => "unknown",
            };
            views.push(DeviceView {
                reference: self.device_ref(id),
                name: display_name(&d.info),
                device_type: d.info.device_type,
                type_text: capitalise(d.info.device_type.spoken()),
                connection: d.info.connection,
                id_kind: d.info.id_kind,
                connected: d.present,
                battery_percent: b.and_then(|b| b.percent),
                charging: b.map(|b| b.charging).unwrap_or_default(),
                battery_source: b.map(|b| b.source).unwrap_or_default(),
                battery_state: state.into(),
                last_seen: self.wall_at_zero + now / 1000,
                prefs: self.settings.prefs(id),
                same_name_as_another: false,
            });
        }
        for (id, k) in &self.known {
            if seen.contains(id) {
                continue;
            }
            let info = DeviceInfo { id: id.clone(), id_kind: k.id_kind, name: k.name.clone(), device_type: k.device_type, manufacturer: String::new(), connection: k.connection };
            views.push(DeviceView {
                reference: self.device_ref(id),
                name: display_name(&info),
                device_type: k.device_type,
                type_text: capitalise(k.device_type.spoken()),
                connection: k.connection,
                id_kind: k.id_kind,
                connected: false,
                battery_percent: None,
                charging: Charging::Unknown,
                battery_source: BatterySource::None,
                battery_state: "unknown".into(),
                last_seen: k.last_seen,
                prefs: self.settings.prefs(id),
                same_name_as_another: false,
            });
        }
        let names: Vec<String> = views.iter().map(|v| v.name.clone()).collect();
        for v in views.iter_mut() {
            v.same_name_as_another = names.iter().filter(|n| **n == v.name).count() > 1;
        }
        views.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)).then(a.reference.cmp(&b.reference)));
        Snapshot {
            version: self.version,
            now_ms: now,
            settings: self.settings.clone(),
            devices: views,
            pending: self.queue.pending_count(),
            sources: self.sources.clone(),
            recent: self.log.iter().rev().take(50).cloned().collect(),
            queue: self.queue.recent(),
            counters: self.counters.clone(),
            warning: self.warning.clone(),
        }
    }

    /// For tests and the status line: the connection state the engine will treat as announced.
    pub fn is_connected(&self, id: &str) -> bool {
        self.devices.get(id).is_some_and(|d| d.present)
    }
}

// `Outcome` above is the decision log's; the battery machine's own enum is imported under another name.
use super::battery::Outcome as Outcome2;
