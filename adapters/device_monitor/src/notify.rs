//! The announcement queue. The core owns it; the interface is a player that asks for the next item and acknowledges it.

use super::battery::Level;
use super::model::*;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnState {
    Pending,
    Delivered,
    Spoken,
    Expired,
    Dropped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Announcement {
    pub id: u64,
    pub priority: Priority,
    pub kind: EventKind,
    #[serde(skip)]
    pub device_ids: Vec<String>,
    pub names: Vec<String>,
    pub text: String,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub state: AnnState,
    #[serde(skip)]
    pub delivered_ms: u64,
    /// How many times it was delivered and could not be spoken (a critical item is tried again).
    #[serde(skip)]
    pub attempts: u8,
    /// For battery items: the level, so the item can be re-checked against the battery just before it is spoken.
    #[serde(skip)]
    pub level: Option<Level>,
}

/// What the player is willing to speak right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allow {
    All,
    CriticalOnly,
}

/// How long a delivered item may wait for its acknowledgement before it is treated as lost.
pub const ACK_TIMEOUT_MS: u64 = 45_000;
/// Connections announced within this long of each other are said in one sentence.
pub const COALESCE_MS: u64 = 3_000;
const KEEP_FINISHED: usize = 50;

#[derive(Debug, Default)]
pub struct Queue {
    items: Vec<Announcement>,
    next_id: u64,
}

impl Queue {
    pub fn push(&mut self, mut a: Announcement) -> u64 {
        self.next_id += 1;
        a.id = self.next_id;
        self.items.push(a);
        self.trim();
        self.next_id
    }

    /// A pending connection item recent enough to be merged with a new one.
    pub fn mergeable_connect(&mut self, now: u64) -> Option<&mut Announcement> {
        self.items.iter_mut().find(|a| a.kind == EventKind::DeviceConnected && a.state == AnnState::Pending && now.saturating_sub(a.created_ms) <= COALESCE_MS)
    }

    pub fn pending_count(&self) -> usize {
        self.items.iter().filter(|a| a.state == AnnState::Pending).count()
    }

    pub fn pending_critical(&self) -> bool {
        self.items.iter().any(|a| a.state == AnnState::Pending && a.priority == Priority::High)
    }

    /// Pending items, best first (higher priority, then older).
    pub fn pending_ids(&self) -> Vec<u64> {
        let mut v: Vec<&Announcement> = self.items.iter().filter(|a| a.state == AnnState::Pending).collect();
        v.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        v.into_iter().map(|a| a.id).collect()
    }

    pub fn get(&self, id: u64) -> Option<&Announcement> {
        self.items.iter().find(|a| a.id == id)
    }

    pub fn set_state(&mut self, id: u64, state: AnnState, now: u64) {
        if let Some(a) = self.items.iter_mut().find(|a| a.id == id) {
            a.state = state;
            if state == AnnState::Delivered {
                a.delivered_ms = now;
            }
        }
    }

    /// Put an item back in the queue after a failed attempt to speak it.
    pub fn retry(&mut self, id: u64) {
        if let Some(a) = self.items.iter_mut().find(|a| a.id == id) {
            a.state = AnnState::Pending;
            a.attempts = a.attempts.saturating_add(1);
        }
    }

    /// Expire what is too old, and put an unacknowledged critical item back so it is spoken again. Returns what changed.
    pub fn expire(&mut self, now: u64) -> Vec<(Announcement, &'static str)> {
        let mut changed = Vec::new();
        for a in self.items.iter_mut() {
            match a.state {
                AnnState::Pending if now >= a.expires_ms && a.priority != Priority::High => {
                    a.state = AnnState::Expired;
                    changed.push((a.clone(), "expired while waiting"));
                }
                AnnState::Delivered if now.saturating_sub(a.delivered_ms) >= ACK_TIMEOUT_MS => {
                    if a.priority == Priority::High {
                        a.state = AnnState::Pending; // at least once for critical
                        changed.push((a.clone(), "not confirmed spoken: will be spoken again"));
                    } else {
                        a.state = AnnState::Dropped;
                        changed.push((a.clone(), "not confirmed spoken: dropped"));
                    }
                }
                _ => {}
            }
        }
        changed
    }

    fn trim(&mut self) {
        let finished = self.items.iter().filter(|a| matches!(a.state, AnnState::Spoken | AnnState::Expired | AnnState::Dropped)).count();
        if finished > KEEP_FINISHED {
            let mut drop_n = finished - KEEP_FINISHED;
            self.items.retain(|a| {
                if drop_n > 0 && matches!(a.state, AnnState::Spoken | AnnState::Expired | AnnState::Dropped) {
                    drop_n -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn recent(&self) -> Vec<Announcement> {
        self.items.iter().rev().take(20).cloned().collect()
    }
}

/// "A", "A and B", "A, B and C".
pub fn join_names(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(kind: EventKind, prio: Priority, at: u64, ttl: u64) -> Announcement {
        Announcement { id: 0, priority: prio, kind, device_ids: vec![], names: vec![], text: "x".into(), created_ms: at, expires_ms: at + ttl, state: AnnState::Pending, delivered_ms: 0, attempts: 0, level: None }
    }

    #[test]
    fn items_come_out_by_priority_then_age() {
        let mut q = Queue::default();
        let low = q.push(ann(EventKind::DeviceDisconnected, Priority::Low, 0, 20_000));
        let med = q.push(ann(EventKind::DeviceConnected, Priority::Medium, 10, 20_000));
        let hi = q.push(ann(EventKind::BatteryCritical, Priority::High, 20, 600_000));
        let med2 = q.push(ann(EventKind::BatteryLow, Priority::Medium, 30, 20_000));
        assert_eq!(q.pending_ids(), vec![hi, med, med2, low]);
    }

    #[test]
    fn old_items_expire_but_a_critical_one_stays_until_it_is_spoken() {
        let mut q = Queue::default();
        let a = q.push(ann(EventKind::DeviceConnected, Priority::Medium, 0, 20_000));
        let c = q.push(ann(EventKind::BatteryCritical, Priority::High, 0, 20_000));
        let ch = q.expire(25_000);
        assert_eq!(ch.len(), 1);
        assert_eq!(q.get(a).unwrap().state, AnnState::Expired);
        assert_eq!(q.get(c).unwrap().state, AnnState::Pending);
    }

    #[test]
    fn an_unacknowledged_critical_item_is_delivered_again_and_others_are_dropped() {
        let mut q = Queue::default();
        let c = q.push(ann(EventKind::BatteryCritical, Priority::High, 0, 600_000));
        let m = q.push(ann(EventKind::BatteryLow, Priority::Medium, 0, 600_000));
        q.set_state(c, AnnState::Delivered, 1_000);
        q.set_state(m, AnnState::Delivered, 1_000);
        assert!(q.expire(10_000).is_empty());
        q.expire(1_000 + ACK_TIMEOUT_MS);
        assert_eq!(q.get(c).unwrap().state, AnnState::Pending);
        assert_eq!(q.get(m).unwrap().state, AnnState::Dropped);
    }

    #[test]
    fn recent_connections_can_be_merged() {
        let mut q = Queue::default();
        q.push(ann(EventKind::DeviceConnected, Priority::Medium, 1_000, 20_000));
        assert!(q.mergeable_connect(3_500).is_some());
        assert!(q.mergeable_connect(4_500).is_none(), "too long ago");
        assert_eq!(join_names(&["Keyboard".into(), "Mouse".into(), "Headphones".into()]), "Keyboard, Mouse and Headphones");
        assert_eq!(join_names(&["A".into(), "B".into()]), "A and B");
    }

    #[test]
    fn finished_items_do_not_pile_up() {
        let mut q = Queue::default();
        for i in 0..200 {
            let id = q.push(ann(EventKind::DeviceConnected, Priority::Medium, i, 10));
            q.set_state(id, AnnState::Spoken, i);
        }
        assert!(q.recent().len() <= 20 && q.items.len() <= KEEP_FINISHED + 1);
    }
}
