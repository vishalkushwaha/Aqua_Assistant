//! Engine tests: scripted events and a virtual clock (milliseconds). No hardware, no sleeping.

use super::battery::Level;
use super::engine::*;
use super::model::*;
use super::notify::*;
use super::settings::*;
use std::collections::BTreeMap;

const S: u64 = 1_000;

fn info(id: &str, name: &str, t: DeviceType) -> DeviceInfo {
    DeviceInfo { id: id.into(), id_kind: IdKind::Address, name: name.into(), device_type: t, manufacturer: String::new(), connection: Connection::Bluetooth }
}
fn buds() -> DeviceInfo {
    info("bt:aa", "Rockerz 551ANC", DeviceType::Headphones)
}
fn kb() -> DeviceInfo {
    DeviceInfo { id: "usb:1:2:3".into(), id_kind: IdKind::UsbSerial, name: "Keychron K2".into(), device_type: DeviceType::Keyboard, manufacturer: String::new(), connection: Connection::Usb }
}
fn bat(p: u8) -> BatteryReading {
    BatteryReading { percent: Some(p), charging: Charging::Unknown, source: BatterySource::WindowsProperty }
}
fn engine() -> Engine {
    let mut e = Engine::new(Settings::default(), BTreeMap::new(), 7, 1_000_000);
    e.begin_baseline(0, BASELINE_MS);
    e
}
fn present(i: DeviceInfo) -> RawEvent {
    RawEvent::Present { info: i, battery: None }
}
fn all() -> DeliveryContext {
    DeliveryContext::default()
}
/// Speak everything that is ready, returning the texts.
fn speak_all(e: &mut Engine, now: u64) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(a) = e.next_for_speaker(now, Allow::All, all()) {
        out.push(a.text.clone());
        e.ack(now, a.id, true);
    }
    out
}
/// A connect that is announced: after the baseline, settled 6 s later.
fn connect(e: &mut Engine, i: DeviceInfo, at: u64) {
    e.ingest(at, present(i));
    e.tick(at + 6 * S);
}
/// Connect, then let the player speak what is ready (as it does within a moment in real use).
fn connect_say(e: &mut Engine, i: DeviceInfo, at: u64) -> Vec<String> {
    connect(e, i, at);
    speak_all(e, at + 7 * S)
}

#[test]
fn devices_already_there_at_start_are_recorded_silently() {
    let mut e = engine();
    e.ingest(1_000, present(buds()));
    e.tick(30 * S);
    assert!(speak_all(&mut e, 30 * S).is_empty());
    assert!(e.is_connected("bt:aa"));
    let snap = e.snapshot(30 * S);
    assert!(snap.recent.iter().any(|d| format!("{:?}", d.outcome).contains("already connected")));
}

#[test]
fn a_new_device_is_announced_once_after_it_has_settled() {
    let mut e = engine();
    e.ingest(30 * S, present(buds()));
    e.tick(31 * S);
    assert!(speak_all(&mut e, 31 * S).is_empty(), "held until it has stayed connected");
    e.tick(35 * S + 100);
    assert_eq!(speak_all(&mut e, 35 * S + 200), vec!["Rockerz 551ANC connected."]);
    // the system repeats the arrival: no second announcement, ever
    e.ingest(40 * S, present(buds()));
    e.ingest(41 * S, present(buds()));
    e.tick(60 * S);
    assert!(speak_all(&mut e, 60 * S).is_empty());
}

#[test]
fn a_device_without_a_usable_name_is_said_by_its_type() {
    let mut e = engine();
    let mut said = connect_say(&mut e, DeviceInfo { name: "HID-compliant mouse".into(), ..info("c:1", "", DeviceType::Mouse) }, 30 * S);
    said.extend(connect_say(&mut e, DeviceInfo { name: String::new(), ..info("c:2", "", DeviceType::UsbStorage) }, 100 * S));
    said.extend(connect_say(&mut e, DeviceInfo { name: String::new(), ..info("c:3", "", DeviceType::UsbUnknown) }, 200 * S));
    assert_eq!(said, vec!["Mouse connected.", "USB storage device connected.", "Unknown USB device connected."]);
}

#[test]
fn a_device_that_flaps_is_announced_once_in_its_final_state_or_not_at_all() {
    let mut e = engine();
    // connected, gone, connected within seconds: announced once
    e.ingest(30 * S, present(buds()));
    e.ingest(32 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.ingest(34 * S, present(buds()));
    e.tick(36 * S);
    assert!(speak_all(&mut e, 36 * S).is_empty(), "not yet: the last change was 2 s ago");
    e.tick(40 * S);
    assert_eq!(speak_all(&mut e, 40 * S), vec!["Rockerz 551ANC connected."]);
    // announced as connected; gone and back within the quiet time: nothing
    e.ingest(100 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.ingest(102 * S, present(buds()));
    e.tick(110 * S);
    assert!(speak_all(&mut e, 110 * S).is_empty());
    // connected then gone before it was ever announced: nothing
    let mut e = engine();
    e.ingest(30 * S, present(kb()));
    e.ingest(31 * S, RawEvent::Gone { id: "usb:1:2:3".into() });
    e.tick(60 * S);
    assert!(speak_all(&mut e, 60 * S).is_empty());
}

#[test]
fn a_flap_that_settles_connected_is_still_announced() {
    // the case the first draft of the plan would have lost
    let mut e = engine();
    for t in [30u64, 31, 32, 33, 34] {
        e.ingest(t * S, RawEvent::Gone { id: "bt:aa".into() });
        e.ingest(t * S + 500, present(buds()));
    }
    e.tick(45 * S);
    assert_eq!(speak_all(&mut e, 45 * S), vec!["Rockerz 551ANC connected."]);
}

#[test]
fn disconnections_are_silent_by_default_and_spoken_when_switched_on() {
    let mut e = engine();
    connect(&mut e, buds(), 30 * S);
    speak_all(&mut e, 40 * S);
    e.ingest(100 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(110 * S);
    assert!(speak_all(&mut e, 110 * S).is_empty());
    let snap = e.snapshot(110 * S);
    assert!(snap.recent.iter().any(|d| format!("{:?}", d.outcome).contains("disconnection announcements are off")));

    let mut e = engine();
    e.set_settings(Settings { announce_disconnections: true, ..Settings::default() }, 0).unwrap();
    connect(&mut e, buds(), 30 * S);
    speak_all(&mut e, 40 * S);
    e.ingest(100 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(110 * S);
    let a = e.next_for_speaker(110 * S, Allow::All, all()).unwrap();
    assert_eq!((a.text.as_str(), a.priority), ("Rockerz 551ANC disconnected.", Priority::Low));
}

#[test]
fn several_connections_close_together_become_one_sentence() {
    let mut e = engine();
    e.ingest(30 * S, present(kb()));
    e.ingest(30 * S + 400, present(buds()));
    e.ingest(30 * S + 800, present(info("c:9", "", DeviceType::Mouse)));
    e.tick(37 * S);
    assert_eq!(speak_all(&mut e, 37 * S), vec!["Keychron K2, Rockerz 551ANC and Mouse connected."]);
}

#[test]
fn switches_work_globally_by_category_by_device_and_by_ignore() {
    // master off
    let mut e = engine();
    e.set_settings(Settings { enabled: false, ..Settings::default() }, 0).unwrap();
    connect(&mut e, buds(), 30 * S);
    assert!(speak_all(&mut e, 40 * S).is_empty());
    // category off
    let mut e = engine();
    let mut s = Settings::default();
    s.disabled_categories.insert(Category::Headphones);
    e.set_settings(s, 0).unwrap();
    let mut said = connect_say(&mut e, buds(), 30 * S);
    said.extend(connect_say(&mut e, kb(), 60 * S));
    assert_eq!(said, vec!["Keychron K2 connected."]);
    // one device ignored: through its opaque reference
    let mut e = engine();
    e.ingest(1 * S, present(buds()));
    let r = e.device_ref("bt:aa");
    e.set_device_prefs(&r, DevicePrefs { ignore: true, ..DevicePrefs::default() }, 2 * S).unwrap();
    e.ingest(30 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(40 * S);
    e.ingest(50 * S, present(buds()));
    e.tick(60 * S);
    assert!(speak_all(&mut e, 60 * S).is_empty());
    let snap = e.snapshot(60 * S);
    assert!(snap.recent.iter().any(|d| format!("{:?}", d.outcome).contains("ignored")), "the reason is on record");
    // a per-device override can turn ON what is off in general (disconnection)
    let mut e = engine();
    connect(&mut e, buds(), 30 * S);
    speak_all(&mut e, 40 * S);
    let r = e.device_ref("bt:aa");
    e.set_device_prefs(&r, DevicePrefs { disconnect: Some(true), ..DevicePrefs::default() }, 41 * S).unwrap();
    e.ingest(100 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(110 * S);
    assert_eq!(speak_all(&mut e, 110 * S), vec!["Rockerz 551ANC disconnected."]);
    // and a per-device "off" beats the general "on"
    let mut e = engine();
    connect(&mut e, kb(), 30 * S);
    speak_all(&mut e, 40 * S);
    e.ingest(50 * S, RawEvent::Gone { id: "usb:1:2:3".into() });
    e.tick(60 * S);
    let r = e.device_ref("usb:1:2:3");
    e.set_device_prefs(&r, DevicePrefs { connect: Some(false), ..DevicePrefs::default() }, 61 * S).unwrap();
    e.ingest(100 * S, present(kb()));
    e.tick(110 * S);
    assert!(speak_all(&mut e, 110 * S).is_empty());
}

#[test]
fn an_ignored_device_that_is_un_ignored_does_not_announce_what_it_missed() {
    let mut e = engine();
    connect(&mut e, buds(), 30 * S);
    speak_all(&mut e, 40 * S);
    let r = e.device_ref("bt:aa");
    e.set_device_prefs(&r, DevicePrefs { ignore: true, ..DevicePrefs::default() }, 41 * S).unwrap();
    e.ingest(50 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(60 * S);
    e.set_device_prefs(&r, DevicePrefs::default(), 61 * S).unwrap();
    e.tick(70 * S);
    assert!(speak_all(&mut e, 70 * S).is_empty(), "its current state is the new baseline");
}

#[test]
fn unknown_usb_announcements_can_be_switched_off_separately() {
    let mut e = engine();
    e.set_settings(Settings { announce_unknown_usb: false, ..Settings::default() }, 0).unwrap();
    let mut said = connect_say(&mut e, DeviceInfo { name: String::new(), ..info("c:3", "", DeviceType::UsbUnknown) }, 30 * S);
    said.extend(connect_say(&mut e, kb(), 60 * S));
    assert_eq!(said, vec!["Keychron K2 connected."]);
}

fn connected_with_battery(e: &mut Engine, p: u8) {
    e.ingest(30 * S, RawEvent::Present { info: buds(), battery: Some(bat(p)) });
    e.tick(36 * S);
    speak_all(e, 36 * S);
}

#[test]
fn a_low_battery_is_confirmed_by_a_re_read_and_announced_once() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    // first low reading: the engine asks for a re-read instead of speaking
    let acts = e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(20) });
    assert_eq!(acts, vec![Action::Reread { id: "bt:aa".into(), after_ms: 3_000 }]);
    assert!(speak_all(&mut e, 101 * S).is_empty());
    // the re-read agrees
    e.ingest(103 * S + 100, RawEvent::Battery { id: "bt:aa".into(), reading: bat(20) });
    assert_eq!(speak_all(&mut e, 104 * S), vec!["Your Rockerz 551ANC has 20 percent battery remaining."]);
    // 19, 18, ... nothing more
    for (i, p) in [19u8, 18, 17, 15].iter().enumerate() {
        e.ingest(200 * S + i as u64 * 300 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(*p) });
        e.tick(200 * S + i as u64 * 300 * S + 5 * S);
    }
    assert!(speak_all(&mut e, 2_000 * S).is_empty());
}

#[test]
fn if_nobody_re_reads_the_engine_asks_again_itself() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(18) });
    assert!(e.tick(101 * S).is_empty());
    assert_eq!(e.tick(103 * S + 500), vec![Action::Reread { id: "bt:aa".into(), after_ms: 0 }]);
}

#[test]
fn critical_battery_is_high_priority_immediate_and_speaks_before_everything_else() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    connect(&mut e, kb(), 100 * S); // a medium connect item waiting
    e.ingest(110 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(5) });
    let first = e.next_for_speaker(111 * S, Allow::All, all()).unwrap();
    assert_eq!((first.priority, first.text.as_str()), (Priority::High, "Warning. Your Rockerz 551ANC is critically low on battery."));
    e.ack(111 * S, first.id, true);
    assert_eq!(speak_all(&mut e, 112 * S), vec!["Keychron K2 connected."]);
}

#[test]
fn skipping_levels_announces_only_the_lowest() {
    let mut e = engine();
    connected_with_battery(&mut e, 24);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(9) });
    e.ingest(103 * S + 100, RawEvent::Battery { id: "bt:aa".into(), reading: bat(9) });
    assert_eq!(speak_all(&mut e, 104 * S), vec!["Your Rockerz 551ANC has 9 percent battery remaining."]);
    e.ingest(200 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(8) });
    e.ingest(203 * S + 100, RawEvent::Battery { id: "bt:aa".into(), reading: bat(8) });
    assert!(speak_all(&mut e, 204 * S).is_empty(), "the Low level is already counted as warned");
}

#[test]
fn a_low_reading_at_connection_is_folded_into_the_connect_sentence() {
    let mut e = engine();
    e.ingest(30 * S, RawEvent::Present { info: buds(), battery: Some(bat(8)) });
    e.tick(36 * S);
    assert_eq!(speak_all(&mut e, 36 * S), vec!["Rockerz 551ANC connected, battery 8 percent."]);
    let mut e = engine();
    e.ingest(30 * S, RawEvent::Present { info: buds(), battery: Some(bat(3)) });
    e.tick(36 * S);
    let a = e.next_for_speaker(36 * S, Allow::All, all()).unwrap();
    assert_eq!((a.priority, a.text.as_str()), (Priority::High, "Rockerz 551ANC connected. Warning. Its battery is critically low."));
    // with connection announcements off, the battery warning still speaks on its own
    let mut e = engine();
    e.set_settings(Settings { announce_connections: false, ..Settings::default() }, 0).unwrap();
    e.ingest(30 * S, RawEvent::Present { info: buds(), battery: Some(bat(8)) });
    e.tick(36 * S);
    assert_eq!(speak_all(&mut e, 36 * S), vec!["Your Rockerz 551ANC has 8 percent battery remaining."]);
}

#[test]
fn a_zero_right_after_connecting_means_not_known_yet() {
    let mut e = engine();
    e.ingest(30 * S, RawEvent::Present { info: buds(), battery: Some(bat(0)) });
    e.tick(36 * S);
    assert_eq!(speak_all(&mut e, 36 * S), vec!["Rockerz 551ANC connected."]);
    // later a real 0 is critical
    e.ingest(300 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(0) });
    assert!(speak_all(&mut e, 301 * S)[0].contains("critically low"));
}

#[test]
fn battery_warnings_can_be_switched_off_by_kind_and_by_device() {
    let mut e = engine();
    e.set_settings(Settings { announce_low_battery: false, ..Settings::default() }, 0).unwrap();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(18) });
    e.ingest(104 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(18) });
    assert!(speak_all(&mut e, 105 * S).is_empty());
    // critical still speaks
    e.ingest(200 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert_eq!(speak_all(&mut e, 201 * S).len(), 1);
    // per device: battery off
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    let r = e.device_ref("bt:aa");
    e.set_device_prefs(&r, DevicePrefs { battery: Some(false), ..DevicePrefs::default() }, 50 * S).unwrap();
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert!(speak_all(&mut e, 101 * S).is_empty());
}

#[test]
fn a_warning_that_is_no_longer_true_is_not_spoken() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(18) });
    e.ingest(103 * S + 100, RawEvent::Battery { id: "bt:aa".into(), reading: bat(18) });
    // before the player asks, the battery rises well above the level (charged)
    e.ingest(110 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(70) });
    assert!(speak_all(&mut e, 111 * S).is_empty());
    assert!(e.snapshot(111 * S).recent.iter().any(|d| format!("{:?}", d.outcome).contains("risen")));
}

#[test]
fn a_reconnection_warns_again_and_a_stale_value_after_a_disconnect_says_nothing() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert_eq!(speak_all(&mut e, 101 * S).len(), 1);
    e.ingest(200 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(210 * S);
    speak_all(&mut e, 210 * S);
    // a late reading for the disconnected device is ignored
    e.ingest(220 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(3) });
    assert!(speak_all(&mut e, 221 * S).is_empty());
    // charged while off, reconnected at 25 percent: no warning; then it falls: warns again
    e.ingest(1_000 * S, RawEvent::Present { info: buds(), battery: Some(bat(25)) });
    e.tick(1_010 * S);
    speak_all(&mut e, 1_010 * S);
    e.ingest(1_500 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert!(speak_all(&mut e, 1_501 * S)[0].contains("critically low"));
}

#[test]
fn a_device_that_disconnects_while_nearly_dead_says_it_may_have_run_out() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    speak_all(&mut e, 101 * S);
    e.ingest(150 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(160 * S);
    let a = e.next_for_speaker(160 * S, Allow::All, all()).unwrap();
    assert_eq!((a.priority, a.text.as_str()), (Priority::High, "Rockerz 551ANC disconnected. It may have run out of battery."));
}

#[test]
fn charging_and_full_are_announced_only_when_switched_on_and_known() {
    let charging = |p| BatteryReading { percent: Some(p), charging: Charging::Charging, source: BatterySource::WindowsProperty };
    let full = BatteryReading { percent: Some(100), charging: Charging::Full, source: BatterySource::WindowsProperty };
    let mut e = engine();
    connected_with_battery(&mut e, 50);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: charging(51) });
    e.ingest(200 * S, RawEvent::Battery { id: "bt:aa".into(), reading: full });
    assert!(speak_all(&mut e, 201 * S).is_empty(), "off by default");
    let mut e = engine();
    e.set_settings(Settings { announce_charging: true, announce_fully_charged: true, ..Settings::default() }, 0).unwrap();
    connected_with_battery(&mut e, 50);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: charging(51) });
    let mut said = speak_all(&mut e, 101 * S);
    e.ingest(110 * S, RawEvent::Battery { id: "bt:aa".into(), reading: charging(52) });
    e.ingest(200 * S, RawEvent::Battery { id: "bt:aa".into(), reading: full });
    said.extend(speak_all(&mut e, 201 * S));
    assert_eq!(said, vec!["Your Rockerz 551ANC is now charging.", "Your Rockerz 551ANC is fully charged."]);
}

#[test]
fn no_low_battery_warning_while_charging_and_the_recovery_can_be_announced() {
    let mut e = engine();
    e.set_settings(Settings { announce_recovered: true, ..Settings::default() }, 0).unwrap();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    speak_all(&mut e, 101 * S);
    let charging = |p| BatteryReading { percent: Some(p), charging: Charging::Charging, source: BatterySource::WindowsProperty };
    e.ingest(200 * S, RawEvent::Battery { id: "bt:aa".into(), reading: charging(6) });
    assert!(speak_all(&mut e, 201 * S).is_empty());
    e.ingest(300 * S, RawEvent::Battery { id: "bt:aa".into(), reading: charging(60) });
    assert_eq!(speak_all(&mut e, 301 * S), vec!["Your Rockerz 551ANC has recovered to 60 percent."]);
}

#[test]
fn the_rate_limit_applies_to_everything_but_high_priority() {
    let mut e = engine();
    e.set_settings(Settings { rate_limit_per_minute: 2, ..Settings::default() }, 0).unwrap();
    let mut said = Vec::new();
    for i in 0..4u64 {
        said.extend(connect_say(&mut e, info(&format!("c:{i}"), &format!("Gadget {i}"), DeviceType::UsbUnknown), 30 * S + i * 10 * S));
    }
    assert_eq!(said.len(), 2);
    assert!(e.snapshot(100 * S).recent.iter().any(|d| format!("{:?}", d.outcome).contains("too many")));
    // critical is never limited
    connected_with_battery(&mut e, 60);
    e.ingest(150 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert_eq!(speak_all(&mut e, 151 * S).len(), 1);
}

#[test]
fn old_items_expire_and_a_critical_one_does_not() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    connect(&mut e, kb(), 40 * S); // a connection that nobody speaks
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    // nobody asks for 2 minutes
    let a = e.next_for_speaker(100 * S + 120 * S, Allow::All, all()).unwrap();
    assert!(a.text.contains("critically"));
    assert_eq!(e.snapshot(300 * S).counters.expired, 1, "the keyboard connection expired while waiting");
}

#[test]
fn quiet_states_hold_everything_but_high_priority() {
    let mut e = engine();
    connect(&mut e, kb(), 30 * S);
    let quiet = DeliveryContext { quiet: true, local_hour: None };
    assert!(e.next_for_speaker(40 * S, Allow::All, quiet).is_none());
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert!(e.next_for_speaker(101 * S, Allow::All, quiet).unwrap().text.contains("critically"));
    // when the quiet ends the held keyboard item (if still fresh) is spoken
    let mut e = engine();
    connect(&mut e, kb(), 30 * S);
    assert!(e.next_for_speaker(40 * S, Allow::All, quiet).is_none());
    assert_eq!(e.next_for_speaker(41 * S, Allow::All, all()).unwrap().text, "Keychron K2 connected.");
    // CriticalOnly (the player is busy speaking an answer)
    let mut e = engine();
    connect(&mut e, kb(), 30 * S);
    assert!(e.next_for_speaker(40 * S, Allow::CriticalOnly, all()).is_none());
}

#[test]
fn quiet_hours_wrap_past_midnight() {
    let mut e = engine();
    e.set_settings(Settings { quiet_hours: QuietHours { enabled: true, start_hour: 22, end_hour: 7 }, ..Settings::default() }, 0).unwrap();
    connect(&mut e, kb(), 30 * S);
    let at = |h| DeliveryContext { quiet: false, local_hour: Some(h) };
    assert!(e.next_for_speaker(40 * S, Allow::All, at(23)).is_none());
    assert!(e.next_for_speaker(41 * S, Allow::All, at(3)).is_none());
    assert!(e.next_for_speaker(42 * S, Allow::All, at(12)).is_some());
}

#[test]
fn an_unconfirmed_critical_item_is_spoken_again_and_others_are_dropped() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    let a = e.next_for_speaker(101 * S, Allow::All, all()).unwrap();
    assert!(e.next_for_speaker(102 * S, Allow::All, all()).is_none(), "delivered, waiting for the acknowledgement");
    // the speaker died: no acknowledgement for 45 s
    e.tick(101 * S + ACK_TIMEOUT_MS);
    let again = e.next_for_speaker(101 * S + ACK_TIMEOUT_MS + 1, Allow::All, all()).unwrap();
    assert_eq!(again.id, a.id);
    e.ack(150 * S, again.id, true);
    assert!(e.next_for_speaker(151 * S, Allow::All, all()).is_none());
}

#[test]
fn a_critical_item_that_could_not_be_spoken_is_tried_again_but_others_are_not() {
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    // the speech engine is busy: four failed attempts are retried
    for i in 0..4u64 {
        let a = e.next_for_speaker(101 * S + i, Allow::All, all()).expect("still pending");
        e.ack(101 * S + i, a.id, false);
    }
    // the fifth failure gives up (it is only a message; the log says so)
    let a = e.next_for_speaker(110 * S, Allow::All, all()).unwrap();
    e.ack(110 * S, a.id, false);
    assert!(e.next_for_speaker(111 * S, Allow::All, all()).is_none());
    // a critical item that is finally spoken is done
    let mut e = engine();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    let a = e.next_for_speaker(101 * S, Allow::All, all()).unwrap();
    e.ack(101 * S, a.id, false);
    let again = e.next_for_speaker(102 * S, Allow::All, all()).unwrap();
    assert_eq!(again.id, a.id);
    e.ack(102 * S, again.id, true);
    assert!(e.next_for_speaker(103 * S, Allow::All, all()).is_none());
    // a medium item that could not be spoken is dropped, not retried
    let mut e = engine();
    connect(&mut e, kb(), 30 * S);
    let m = e.next_for_speaker(37 * S, Allow::All, all()).unwrap();
    e.ack(37 * S, m.id, false);
    assert!(e.next_for_speaker(38 * S, Allow::All, all()).is_none());
}

#[test]
fn waking_from_sleep_does_not_announce_devices_that_were_connected_before() {
    let mut e = engine();
    connect(&mut e, buds(), 30 * S);
    speak_all(&mut e, 40 * S);
    e.note_resume(1_000 * S);
    // the headset comes back 20 s after the wake
    e.ingest(1_005 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.ingest(1_020 * S, present(buds()));
    e.tick(1_040 * S);
    assert!(speak_all(&mut e, 1_040 * S).is_empty());
    // a different device that is new still is announced
    connect(&mut e, kb(), 1_050 * S);
    assert_eq!(speak_all(&mut e, 1_060 * S), vec!["Keychron K2 connected."]);
    // after the grace period the same headset is a normal reconnection
    e.ingest(1_300 * S, RawEvent::Gone { id: "bt:aa".into() });
    e.tick(1_310 * S);
    e.ingest(1_400 * S, present(buds()));
    e.tick(1_410 * S);
    assert_eq!(speak_all(&mut e, 1_410 * S), vec!["Rockerz 551ANC connected."]);
}

#[test]
fn the_computers_own_battery_warns_and_never_announces_a_connection() {
    let computer = DeviceInfo { id: "system:battery".into(), id_kind: IdKind::System, name: String::new(), device_type: DeviceType::Computer, manufacturer: String::new(), connection: Connection::Internal };
    let sys = |p, c| BatteryReading { percent: Some(p), charging: c, source: BatterySource::System };
    let mut e = engine();
    e.ingest(50 * S, RawEvent::Present { info: computer.clone(), battery: Some(sys(80, Charging::Discharging)) });
    e.tick(60 * S);
    assert!(speak_all(&mut e, 60 * S).is_empty());
    e.ingest(300 * S, RawEvent::Battery { id: "system:battery".into(), reading: sys(20, Charging::Discharging) });
    e.ingest(304 * S, RawEvent::Battery { id: "system:battery".into(), reading: sys(20, Charging::Discharging) });
    let mut said = speak_all(&mut e, 305 * S);
    e.ingest(900 * S, RawEvent::Battery { id: "system:battery".into(), reading: sys(4, Charging::Discharging) });
    said.extend(speak_all(&mut e, 901 * S));
    assert_eq!(said, vec!["Your computer has 20 percent battery remaining.", "Warning. Your computer is critically low on battery."]);
}

#[test]
fn a_critical_warning_can_be_repeated_when_the_user_asks_for_it() {
    let mut e = engine();
    e.set_settings(Settings { repeat_critical_minutes: 5, ..Settings::default() }, 0).unwrap();
    connected_with_battery(&mut e, 60);
    e.ingest(100 * S, RawEvent::Battery { id: "bt:aa".into(), reading: bat(4) });
    assert_eq!(speak_all(&mut e, 101 * S).len(), 1);
    e.tick(100 * S + 4 * 60 * S);
    assert!(speak_all(&mut e, 100 * S + 4 * 60 * S).is_empty());
    e.tick(100 * S + 5 * 60 * S + 1);
    assert_eq!(speak_all(&mut e, 100 * S + 5 * 60 * S + 2).len(), 1);
}

#[test]
fn the_snapshot_exposes_opaque_references_not_ids_and_lists_remembered_devices() {
    let mut known = BTreeMap::new();
    known.insert("bt:ff".into(), Known { name: "Old Buds".into(), device_type: DeviceType::Headphones, connection: Connection::Bluetooth, id_kind: IdKind::Address, last_seen: 5 });
    let mut e = Engine::new(Settings::default(), known, 99, 1_000_000);
    e.begin_baseline(0, BASELINE_MS);
    e.ingest(1 * S, present(buds()));
    e.ingest(1 * S, present(info("bt:bb", "Rockerz 551ANC", DeviceType::Headphones)));
    let snap = e.snapshot(2 * S);
    let json = serde_json::to_string(&snap).unwrap();
    assert!(!json.contains("bt:aa") && !json.contains("bt:ff"), "ids and addresses stay inside the core");
    assert_eq!(snap.devices.len(), 3);
    assert!(snap.devices.iter().filter(|d| d.connected).count() == 2);
    assert!(snap.devices.iter().filter(|d| d.name == "Rockerz 551ANC").all(|d| d.same_name_as_another));
    let old = snap.devices.iter().find(|d| d.name == "Old Buds").unwrap();
    assert!(!old.connected && old.battery_state == "unknown");
    // forget by reference
    assert!(e.forget(&old.reference.clone(), 3 * S));
    assert_eq!(e.snapshot(4 * S).devices.len(), 2);
    assert!(!e.forget("nonexistent", 5 * S));
}

#[test]
fn bad_settings_are_refused_and_the_old_ones_kept() {
    let mut e = engine();
    let bad = Settings { low_percent: 5, very_low_percent: 10, critical_percent: 20, ..Settings::default() };
    assert!(e.set_settings(bad, 0).is_err());
    assert_eq!(e.settings().low_percent, 20);
}

#[test]
fn the_test_announcement_is_a_normal_queued_item() {
    let mut e = engine();
    let id = e.test_announcement(10 * S);
    let a = e.next_for_speaker(11 * S, Allow::All, all()).unwrap();
    assert_eq!(a.id, id);
    assert!(a.text.contains("test"));
}

#[test]
fn battery_level_names_in_the_log_use_the_engines_own_levels() {
    // a small guard that the public level type stays in sync with the thresholds
    let th = crate::battery::Thresholds { low: 20, very_low: 10, critical: 5 };
    assert_eq!(th.level_of(5), Level::Critical);
}
