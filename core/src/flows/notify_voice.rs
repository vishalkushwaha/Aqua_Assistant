//! A notification that is said aloud: the `voice` channel of flows (card R6-06, task T6-06.9).
//!
//! A reminder that comes due is announced through the channels its schedule asks for (`in_app` always, and `voice` for the reminders Aqua sets: see `reminders::Live`). The channel
//! does not speak: it puts a short sentence in a queue, and the app's player takes it from `POST /api/v1/announcements/next`, says it with the speech engine, and answers
//! `POST /api/v1/announcements/{id}/ack` (see `api_gateway/announce_api.rs`, which is modelled on the device announcements). The notification itself stays in the app's list, so a
//! reminder that nobody heard is still there to be read.
//!
//! The queue is small and in memory. It is the moment that matters for something said aloud: an announcement older than an hour is not said, at most
//! [`MAX_WAITING`] wait (the oldest is dropped), one that the player took and did not say is offered again [`MAX_TRIES`] times in all, and one that was taken is not offered to a
//! second player for [`LEASE_SECS`] seconds. Nothing here starts the speech engine or the player; the player and the toast belong to the app.

use super::notify::{Channel, Notification};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// The id the channel has in `NewNotification` channels and in `GET /api/v1/flows/notifications/channels`.
pub const CHANNEL_ID: &str = "voice";

/// How many announcements may wait. A reminder set long ago that nobody was there to hear is not worth the room of a new one.
pub const MAX_WAITING: usize = 20;
/// An announcement older than this (seconds) is not said any more: "your tea is ready" an hour late is noise.
pub const MAX_AGE_SECS: i64 = 3_600;
/// How long a player may hold an announcement before it is offered again (seconds).
pub const LEASE_SECS: i64 = 45;
/// How many times one announcement is offered in all (the player may fail to speak it).
pub const MAX_TRIES: u8 = 3;
/// The player is "connected" when it asked for something within this many seconds.
pub const PLAYER_SEEN_SECS: i64 = 120;
/// The longest sentence that is said aloud (characters).
pub const MAX_SPOKEN_CHARS: usize = 240;

/// One thing to say.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Announcement {
    pub id: u64,
    /// The sentence to say, as it is to be spoken.
    pub text: String,
    /// The kind of notification it comes from (`reminder`, `succeeded`, ...), so the app can show it as a toast too.
    pub kind: String,
    /// When it was made (unix seconds).
    pub at: i64,
}

struct Waiting {
    announcement: Announcement,
    tries: u8,
    /// Until this moment (unix seconds) a player holds it.
    leased_until: i64,
}

#[derive(Default)]
struct Inner {
    items: VecDeque<Waiting>,
    next_id: u64,
    /// When a player last asked (unix seconds), 0 for never.
    player_seen: i64,
}

/// The announcements that wait to be said. The clock is given by the caller (unix seconds), so that a test does not wait.
#[derive(Default)]
pub struct VoiceQueue {
    inner: Mutex<Inner>,
}

impl VoiceQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a sentence to say. The oldest waiting one goes when the queue is full. Returns the id.
    pub fn push(&self, text: String, kind: &str, now: i64) -> u64 {
        let mut q = self.lock();
        q.next_id += 1;
        let id = q.next_id;
        q.items.push_back(Waiting { announcement: Announcement { id, text, kind: kind.to_string(), at: now }, tries: 0, leased_until: 0 });
        while q.items.len() > MAX_WAITING {
            q.items.pop_front();
        }
        id
    }

    /// Adds a sentence to say in place of the one of the same `kind` that still waits (one that no player holds): a thing that is said once, however many times it happened while nobody
    /// was listening ("I need your yes on 3 things" is not three sentences). Returns the id (card OA10-03, UX-13).
    pub fn push_replacing(&self, text: String, kind: &str, now: i64) -> u64 {
        {
            let mut q = self.lock();
            q.items.retain(|w| !(w.announcement.kind == kind && w.leased_until <= now));
        }
        self.push(text, kind, now)
    }

    /// Takes the sentences of this `kind` that no player holds out of the queue (what was to be said is not true any more: the question was answered). Returns how many (card OA10-03, UX-13).
    pub fn withdraw(&self, kind: &str, now: i64) -> usize {
        let mut q = self.lock();
        let before = q.items.len();
        q.items.retain(|w| !(w.announcement.kind == kind && w.leased_until <= now));
        before - q.items.len()
    }

    /// The next thing the player may say, or `None`. The player holds it for [`LEASE_SECS`] and answers with [`ack`](Self::ack). A player that asks is a player that is there, whether
    /// or not there is anything to say.
    pub fn next(&self, now: i64) -> Option<Announcement> {
        let mut q = self.lock();
        q.player_seen = now;
        q.items.retain(|w| now - w.announcement.at <= MAX_AGE_SECS);
        let w = q.items.iter_mut().find(|w| w.leased_until <= now)?;
        w.leased_until = now + LEASE_SECS;
        w.tries += 1;
        Some(w.announcement.clone())
    }

    /// The player's answer: `spoken` removes it; not spoken offers it again, unless it has been offered [`MAX_TRIES`] times. Returns whether the id was waiting.
    pub fn ack(&self, id: u64, spoken: bool, now: i64) -> bool {
        let mut q = self.lock();
        q.player_seen = now;
        let Some(at) = q.items.iter().position(|w| w.announcement.id == id) else { return false };
        if spoken || q.items[at].tries >= MAX_TRIES {
            q.items.remove(at);
        } else {
            q.items[at].leased_until = 0;
        }
        true
    }

    /// How many are waiting (held ones included).
    pub fn waiting(&self) -> usize {
        self.lock().items.len()
    }

    /// A player asked and was given nothing (it is busy): it is there all the same.
    pub fn note_player(&self, now: i64) {
        self.lock().player_seen = now;
    }

    /// Whether a player asked lately: something that can say them is there.
    pub fn player_is_there(&self, now: i64) -> bool {
        let seen = self.lock().player_seen;
        seen > 0 && now - seen <= PLAYER_SEEN_SECS
    }
}

/// The queue of this process: what the channel fills and the routes empty.
pub fn queue() -> Arc<VoiceQueue> {
    static QUEUE: OnceLock<Arc<VoiceQueue>> = OnceLock::new();
    QUEUE.get_or_init(Default::default).clone()
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The sentence to say for a notification: a reminder says what it is for ("Reminder: call Anna."), anything else says its title without the picture in front of it. One line, cut
/// at [`MAX_SPOKEN_CHARS`]. Empty when there is nothing to say.
pub fn spoken_text(n: &Notification) -> String {
    let raw = if n.kind == "reminder" {
        let what = n.body.trim();
        if what.is_empty() {
            String::new()
        } else if what == crate::reminders::TIMER_MESSAGE {
            what.to_string()
        } else {
            format!("Reminder: {what}")
        }
    } else {
        n.title.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string()
    };
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one_line.chars().take(MAX_SPOKEN_CHARS).collect();
    if !out.is_empty() && !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// The `voice` channel of flows: a notification that asks for it is put in a queue to be said by the app's player.
pub struct VoiceChannel {
    queue: Arc<VoiceQueue>,
}

impl VoiceChannel {
    /// The channel of this process (it fills [`queue`]).
    pub fn live() -> Self {
        Self { queue: queue() }
    }

    /// A channel with a queue of its own (a test).
    pub fn with_queue(queue: Arc<VoiceQueue>) -> Self {
        Self { queue }
    }
}

impl Channel for VoiceChannel {
    fn id(&self) -> &'static str {
        CHANNEL_ID
    }
    fn label(&self) -> &'static str {
        "Spoken announcements"
    }
    /// Connected when the app's player asked for announcements in the last two minutes: until then nothing can say them.
    fn connected(&self) -> bool {
        self.queue.player_is_there(unix_now())
    }
    /// Always queued, so that it is said when the app comes (within the hour). The note says so when no player is there now, which the settings page shows as it does for
    /// an account that is not connected.
    fn deliver(&self, n: &Notification) -> Result<(), String> {
        let text = spoken_text(n);
        if text.is_empty() {
            return Err("there was nothing to say".to_string());
        }
        let now = unix_now();
        self.queue.push(text, &n.kind, now);
        if self.queue.player_is_there(now) {
            Ok(())
        } else {
            Err("queued: the app is not listening for announcements, so it will be said when the app is open".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::notify::{NewNotification, Notifier, NotifyStore};
    use serde_json::Value;

    const T0: i64 = 1_900_000_000;

    fn notifier(name: &str, voice: Arc<VoiceQueue>) -> (Notifier, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("aqua_notify_voice_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let notes = Arc::new(NotifyStore::new(dir.join("flows.db")));
        (Notifier::new(notes).with_channel(Arc::new(VoiceChannel::with_queue(voice))), dir)
    }

    fn reminder(message: &str) -> NewNotification {
        NewNotification { kind: "reminder", level: "info", flow_id: 1, run_id: Some(1), schedule_id: Some(1), title: format!("⏰ {message}"), body: message.to_string(), data: serde_json::json!({ "reminder": true }) }
    }

    #[test]
    fn a_reminder_that_comes_due_is_kept_in_the_app_and_put_in_the_queue_to_be_said_with_what_it_is_for() {
        let voice = Arc::new(VoiceQueue::default());
        let (notifier, dir) = notifier("due", voice.clone());
        let channels = ["in_app".to_string(), "voice".to_string()];
        let n = notifier.emit("local", &channels, reminder("call Anna"), T0).unwrap();
        assert_eq!(n.delivery.iter().map(|d| d.channel.as_str()).collect::<Vec<_>>(), ["in_app", "voice"]);
        assert!(n.delivery[0].ok, "the notification is in the app whatever happens to the sentence");
        assert_eq!(voice.waiting(), 1);
        let said = voice.next(unix_now()).expect("the player is given it");
        assert_eq!((said.text.as_str(), said.kind.as_str()), ("Reminder: call Anna.", "reminder"));
        assert_eq!(notifier.store.list("local", false, 10).unwrap().len(), 1, "still in the list of notifications");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nobody_listening_is_said_in_the_delivery_note_and_the_sentence_waits_for_the_app() {
        let voice = Arc::new(VoiceQueue::default());
        let (notifier, dir) = notifier("nobody", voice.clone());
        let n = notifier.emit("local", &["in_app".to_string(), "voice".to_string()], reminder("stretch"), T0).unwrap();
        let spoken = n.delivery.iter().find(|d| d.channel == "voice").unwrap();
        assert!(!spoken.ok && spoken.note.starts_with("queued:"), "{spoken:?}");
        assert_eq!(voice.waiting(), 1, "it is still there for when the player comes");
        assert!(!notifier.channel_status().iter().find(|c| c.id == "voice").unwrap().connected, "no player has asked: not connected");
        // The player comes: the channel is connected, and the next reminder is delivered as heard-able.
        assert!(voice.next(unix_now()).is_some());
        assert!(notifier.channel_status().iter().find(|c| c.id == "voice").unwrap().connected);
        let n = notifier.emit("local", &["voice".to_string()], reminder("water"), unix_now()).unwrap();
        assert!(n.delivery.iter().find(|d| d.channel == "voice").unwrap().ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_the_channel_registered_voice_is_an_unknown_channel_and_nothing_is_queued() {
        let dir = std::env::temp_dir().join(format!("aqua_notify_voice_plain_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plain = Notifier::new(Arc::new(NotifyStore::new(dir.join("flows.db"))));
        let n = plain.emit("local", &["in_app".to_string(), "voice".to_string()], reminder("call Anna"), T0).unwrap();
        let spoken = n.delivery.iter().find(|d| d.channel == "voice").unwrap();
        assert_eq!((spoken.ok, spoken.note.as_str()), (false, "unknown channel"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_taken_announcement_is_not_offered_twice_until_it_is_not_spoken_and_it_goes_when_spoken() {
        let q = VoiceQueue::default();
        let id = q.push("Reminder: call Anna.".into(), "reminder", T0);
        let first = q.next(T0 + 1).expect("offered");
        assert_eq!(first.id, id);
        assert!(q.next(T0 + 2).is_none(), "held by the player for {LEASE_SECS} seconds");
        // The player could not speak it: offered again at once.
        assert!(q.ack(id, false, T0 + 3));
        assert_eq!(q.next(T0 + 4).map(|a| a.id), Some(id));
        // A player that took it and vanished: offered again when the lease is over.
        assert!(q.next(T0 + 4 + LEASE_SECS - 1).is_none());
        assert_eq!(q.next(T0 + 4 + LEASE_SECS).map(|a| a.id), Some(id));
        // Spoken: gone, and an answer for it again is an unknown id.
        assert!(q.ack(id, true, T0 + 60));
        assert_eq!((q.waiting(), q.ack(id, true, T0 + 61), q.next(T0 + 62)), (0, false, None));
    }

    #[test]
    fn one_that_could_not_be_spoken_is_given_up_after_three_tries_and_an_old_one_is_not_said() {
        let q = VoiceQueue::default();
        let id = q.push("Reminder: a.".into(), "reminder", T0);
        for round in 0..MAX_TRIES {
            assert_eq!(q.next(T0 + i64::from(round)).map(|a| a.id), Some(id), "try {round}");
            q.ack(id, false, T0 + i64::from(round));
        }
        assert_eq!(q.waiting(), 0, "three tries and it is let go");
        // An hour late is not said.
        q.push("Reminder: b.".into(), "reminder", T0);
        assert!(q.next(T0 + MAX_AGE_SECS + 1).is_none());
        assert_eq!(q.waiting(), 0);
    }

    #[test]
    fn a_thing_that_happened_again_while_nobody_listened_is_said_once_and_one_that_was_answered_is_not_said() {
        let q = VoiceQueue::default();
        q.push_replacing("I need your yes to change a file. It is on the screen.".into(), "approval_waiting", T0);
        q.push_replacing("I need your yes on 2 things. They are on the screen.".into(), "approval_waiting", T0 + 1);
        q.push("Reminder: call Anna.".into(), "reminder", T0 + 1);
        assert_eq!(q.waiting(), 2, "the second sentence took the place of the first, and a reminder is not touched");
        assert_eq!(q.next(T0 + 2).unwrap().text, "I need your yes on 2 things. They are on the screen.");
        // A sentence a player holds is being said: it is not taken back, and a new one waits behind it.
        q.push_replacing("I need your yes on 3 things. They are on the screen.".into(), "approval_waiting", T0 + 3);
        assert_eq!(q.waiting(), 3);
        // Withdrawn: what waits and is of that kind goes; what a player holds, and the reminder, stay.
        assert_eq!(q.withdraw("approval_waiting", T0 + 4), 1);
        assert_eq!(q.waiting(), 2);
        assert_eq!(q.withdraw("approval_waiting", T0 + 4), 0);
    }

    #[test]
    fn at_most_twenty_wait_and_the_oldest_is_the_one_dropped() {
        let q = VoiceQueue::default();
        for i in 0..(MAX_WAITING + 5) {
            q.push(format!("Reminder: {i}."), "reminder", T0);
        }
        assert_eq!(q.waiting(), MAX_WAITING);
        assert_eq!(q.next(T0).unwrap().text, "Reminder: 5.", "the first five were dropped");
    }

    #[test]
    fn what_is_said_is_one_short_sentence_without_the_picture_and_a_timer_says_it_is_done() {
        let n = |kind: &'static str, title: &str, body: &str| Notification {
            id: 1,
            owner_id: "local".into(),
            at: T0,
            kind: kind.into(),
            level: "info".into(),
            flow_id: 1,
            run_id: None,
            schedule_id: None,
            title: title.into(),
            body: body.into(),
            data: Value::Null,
            read: false,
            delivery: Vec::new(),
        };
        assert_eq!(spoken_text(&n("reminder", "⏰ call Anna", "call Anna")), "Reminder: call Anna.");
        assert_eq!(spoken_text(&n("reminder", "⏰ Your timer is done", crate::reminders::TIMER_MESSAGE)), "Your timer is done.");
        assert_eq!(spoken_text(&n("failed", "⚠️ Morning post could not be completed", "long body that is not read")), "Morning post could not be completed.");
        assert_eq!(spoken_text(&n("reminder", "⏰", "  ")), "", "nothing to say");
        let long = spoken_text(&n("reminder", "", &"word ".repeat(200)));
        assert!(long.chars().count() <= MAX_SPOKEN_CHARS + 1 && !long.contains('\n'), "{long}");
        assert_eq!(spoken_text(&n("reminder", "", "line one\nline two")), "Reminder: line one line two.");
    }
}
