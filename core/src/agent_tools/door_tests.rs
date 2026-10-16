//! Tests of `door.rs`: the comparison of the one door with the old routes, and the calls of the browser and the desktop through the host (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::door_tests::<name>`).

use super::tests::host;
use super::*;
use aqua_tool_host::*;
use serde_json::json;
use std::sync::{Mutex, PoisonError};

#[test]
fn what_the_old_route_did_is_read_from_its_text() {
    assert_eq!(legacy_outcome("Page Title: Example
Hello"), "ran");
    assert_eq!(legacy_outcome(""), "ran");
    assert_eq!(legacy_outcome("ERROR [POLICY_TOOL_BLOCKED] browser_click: a voice request can read pages"), "refused");
    assert_eq!(legacy_outcome("  ERROR [POLICY_BLOCKED_ORIGIN] local_open_url: no"), "refused");
    assert_eq!(legacy_outcome("ERROR [APPROVAL_DENIED] browser_type: not approved"), "refused");
    assert_eq!(legacy_outcome("ERROR [BACKEND_ERROR] the page did not load"), "failed");
    assert_eq!(legacy_outcome("ERROR timed out"), "failed");
}

/// The comparison's counters are process-wide (`metrics`, `door_shadow`): the tests that read them or write to them take turns, or one's count moves under the other's assertion.
static DOOR_COUNTS: Mutex<()> = Mutex::new(());

#[test]
fn the_comparison_counts_what_the_host_would_have_said_beside_what_ran_and_a_missing_preview_counts_nothing() {
    let _turn = DOOR_COUNTS.lock().unwrap_or_else(PoisonError::into_inner);
    let count = |tool: &str, host: &str, legacy: &str| crate::metrics::counter_value("aqua_one_door_total", &[("tool", tool), ("host", host), ("legacy", legacy)]);
    let before = count("browser.click", "ask", "ran");
    one_door_compare(Some(DoorPreview { tool: "browser.click", decision: Some(Decision::Ask), door: Some(Decision::Ask), questions_before: u64::MAX }), "clicked");
    assert_eq!(count("browser.click", "ask", "ran"), before + 1.0);
    let before = count("win.type", "no_preview", "refused");
    one_door_compare(Some(DoorPreview { tool: "win.type", decision: None, door: None, questions_before: u64::MAX }), "ERROR [POLICY_TOOL_BLOCKED] Type: no");
    assert_eq!(count("win.type", "no_preview", "refused"), before + 1.0);
    let total = crate::metrics::counter_total("aqua_one_door_total");
    one_door_compare(None, "anything");
    assert_eq!(crate::metrics::counter_total("aqua_one_door_total"), total, "no preview, nothing counted");
}

/// Card R6-08: the door may be switched on only if it is never looser than the old route, and the old route's questions are not in its text, so they are counted on the
/// run's thread. Here the old route "asked" by a question put on this thread between the two readings.
#[test]
fn a_door_that_would_have_been_looser_than_the_old_route_is_counted_and_one_that_is_not_is_not() {
    let _turn = DOOR_COUNTS.lock().unwrap_or_else(PoisonError::into_inner);
    let looser = |tool: &str, door: &str, legacy: &str| crate::metrics::counter_value("aqua_one_door_looser_total", &[("tool", tool), ("door", door), ("legacy", legacy)]);
    let broker = aqua_mcp_adapters::browser::ConfirmationBroker::default();
    let ask = || {
        let _ = broker.request(aqua_mcp_adapters::browser::ConfirmKind::Action, "browser_click", "x".into(), None, None, std::time::Duration::from_millis(10));
    };
    let preview = |tool: &'static str, door: Decision, questions_before: u64| DoorPreview { tool, decision: Some(Decision::Auto), door: Some(door), questions_before };
    // The old route asked and the door says "auto": looser. The door says "ask": not.
    let before = crate::door_shadow::take_question_baseline();
    ask();
    let (a, b) = (looser("browser.hover", "auto", "asked"), looser("browser.dialog", "ask", "asked"));
    one_door_compare(Some(preview("browser.hover", Decision::Auto, before)), "hovered");
    one_door_compare(Some(preview("browser.dialog", Decision::Ask, before)), "dismissed");
    assert_eq!((looser("browser.hover", "auto", "asked"), looser("browser.dialog", "ask", "asked")), (a + 1.0, b));
    // A rule refused it without a question and the door would have said "ask": looser; the door that says "deny" is not.
    let refusal = "ERROR [PERMISSION_DENIED] Type: refusing to type in Aqua's own window";
    let (c, d) = (looser("win.multi_edit", "ask", "refused"), looser("win.multi_select", "deny", "refused"));
    one_door_compare(Some(preview("win.multi_edit", Decision::Ask, u64::MAX)), refusal);
    one_door_compare(Some(preview("win.multi_select", Decision::Deny, u64::MAX)), refusal);
    assert_eq!((looser("win.multi_edit", "ask", "refused"), looser("win.multi_select", "deny", "refused")), (c + 1.0, d));
    // It ran with no question and the door would have asked: stricter, not looser. And what the counts keep names the tool and the words, nothing of the call.
    let e = looser("win.wait", "always_ask", "ran");
    one_door_compare(Some(preview("win.wait", Decision::AlwaysAsk, u64::MAX)), "waited");
    assert_eq!(looser("win.wait", "always_ask", "ran"), e);
    let tally = crate::door_shadow::tally_so_far();
    assert!(tally.looser.get("browser.hover|auto|auto|asked").is_some_and(|n| *n >= 1), "{:?}", tally.looser);
    assert!(tally.counts.contains_key("win.wait|auto|always_ask|ran"), "{:?}", tally.counts);
}

/// Card R6-08: `door_call` sends a call to the host only when the door is on, there is a host, and the name is a browser or desktop tool as the model writes it.
#[test]
fn the_door_takes_a_browser_or_desktop_call_only_when_it_is_on_there_is_a_host_and_the_name_is_one_of_those_tools() {
    use crate::door_shadow::DoorMode;
    let h = host();
    let call = |mode: DoorMode, name: &str| door_call(mode, Some(&h), name, json!({}), Caller::Agent, "run_door");
    for mode in [DoorMode::Shadow, DoorMode::Off] {
        assert_eq!(call(mode, "browser_click"), None, "{mode:?}: the caller takes its own route");
        assert_eq!(call(mode, "Click"), None, "{mode:?}");
    }
    for name in ["fs.read_file", "fs_read_file", "local_find", "local_open_url", "PowerShell", " Click", "", "browser.click", "win.click"] {
        assert_eq!(call(DoorMode::On, name), None, "{name:?} is not a browser or desktop tool as the model names them");
    }
    assert_eq!(door_call(DoorMode::On, None, "browser_click", json!({}), Caller::Agent, "run_door"), None, "no host");
    // A browser or desktop tool: the host answers, in the words any host tool is answered in (this host has no such providers, so it says so rather than leaving the call to
    // a route that would not have been asked).
    for name in ["browser_click", "Click", "click", "DisplayInventory"] {
        let text = call(DoorMode::On, name).unwrap_or_else(|| panic!("{name}"));
        assert!(text.starts_with("ERROR [NOT_FOUND]"), "{name}: {text}");
    }
}

#[test]
fn a_call_with_no_host_name_has_no_preview() {
    // `local_find` is not a browser or desktop tool, and an empty name is nothing.
    assert!(one_door_preview("local_find", &json!({}), Caller::Agent).is_none());
    assert!(one_door_preview("", &json!({}), Caller::Agent).is_none());
}
