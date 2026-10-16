use super::*;
use aqua_mcp_adapters::browser::{ConfirmKind, ConfirmationBroker};
use axum::http::StatusCode;
use std::time::Duration;

fn open_one(b: &Arc<ConfirmationBroker>, kind: ConfirmKind, always_ask: bool, wait: Duration) -> (String, std::thread::JoinHandle<Decision>) {
    let b2 = b.clone();
    let asker = std::thread::spawn(move || b2.request_detailed(kind, "term.run", "run a command".into(), None, None, always_ask, vec![], wait).1);
    for _ in 0..500 {
        if let Some(r) = b.pending().into_iter().next() {
            return (r.request_id, asker);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no question was asked");
}

#[test]
fn an_answer_is_one_of_a_closed_set_of_words_and_says_what_became_of_its_question() {
    let b = Arc::new(ConfirmationBroker::default());
    // Words that are not answers are refused with what is accepted, and nothing is guessed (a spoken "yeah sure" does not approve anything).
    let (id, asker) = open_one(&b, ConfirmKind::Tool, false, Duration::from_secs(30));
    for word in ["yeah sure", "", "ALLOW EVERYTHING", "approve; deny"] {
        let (code, body) = answer_confirmation(&b, &id, word);
        assert_eq!(code, StatusCode::BAD_REQUEST, "{word:?}: {body}");
        assert_eq!(body["choices"], serde_json::json!(["approve", "approve_always", "deny"]));
    }
    assert_eq!(b.pending().len(), 1, "the question is still open after the words that were not answers");
    // `edit` is a word that is known and not offered.
    let (code, body) = answer_confirmation(&b, &id, "edit");
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"].as_str().unwrap().contains("not offered"), "{body}");
    assert_eq!(b.pending().len(), 1);
    // A word in another case, and the older name of it, is an answer.
    let (code, body) = answer_confirmation(&b, &id, " Allow ");
    assert_eq!((code, body["answer"].clone()), (StatusCode::OK, serde_json::json!("approve")), "{body}");
    assert_eq!(asker.join().unwrap(), Decision::Allow);
    // The same question, answered again, is answered already.
    let (code, _) = answer_confirmation(&b, &id, "deny");
    assert_eq!(code, StatusCode::CONFLICT);
    // A question nobody asked.
    assert_eq!(answer_confirmation(&b, "c_nothing", "approve").0, StatusCode::NOT_FOUND);
}

#[test]
fn a_yes_that_comes_after_the_question_expired_is_told_it_did_nothing() {
    let b = Arc::new(ConfirmationBroker::default());
    let (id, asker) = open_one(&b, ConfirmKind::Tool, false, Duration::from_millis(200));
    assert_eq!(asker.join().unwrap(), Decision::Deny, "nobody answered in time");
    let (code, body) = answer_confirmation(&b, &id, "approve");
    assert_eq!(code, StatusCode::GONE, "{body}");
    assert!(body["expired"] == true && body["error"].as_str().unwrap().contains("nothing was done"), "{body}");
}

#[test]
fn approving_for_good_is_only_for_a_question_that_may_be_remembered() {
    let b = Arc::new(ConfirmationBroker::default());
    // A question that is asked every time, and a browser action: approve once or deny.
    for (kind, always_ask) in [(ConfirmKind::Tool, true), (ConfirmKind::Action, false)] {
        let (id, asker) = open_one(&b, kind, always_ask, Duration::from_secs(30));
        let (code, body) = answer_confirmation(&b, &id, "approve_always");
        assert_eq!(code, StatusCode::BAD_REQUEST, "{kind:?}: {body}");
        assert_eq!(body["choices"], serde_json::json!(["approve", "deny"]));
        assert_eq!(answer_confirmation(&b, &id, "deny").0, StatusCode::OK);
        assert_eq!(asker.join().unwrap(), Decision::Deny);
    }
    // An unknown site, and a tool question that may be remembered: allowed, by either name.
    for (kind, word) in [(ConfirmKind::Site, "approve_always"), (ConfirmKind::Tool, "allow_always")] {
        let (id, asker) = open_one(&b, kind, false, Duration::from_secs(30));
        let (code, body) = answer_confirmation(&b, &id, word);
        assert_eq!((code, body["answer"].clone()), (StatusCode::OK, serde_json::json!("approve_always")), "{body}");
        assert_eq!(asker.join().unwrap(), Decision::AllowAlways);
    }
}

/// Card OA10-03 (AG-11): a "no" can come with words. They are one line of at most 300 characters with secrets masked, they go with a "no" only (with a yes the answer is refused, so words
/// beside a yes cannot be taken for an instruction), and they are kept for the run only when `AQUA_DENY_FEEDBACK` is on: with it off the answer is the same "no" and says the words were
/// not kept.
#[test]
fn words_come_with_a_no_only_are_one_short_line_and_are_kept_only_when_the_setting_is_on() {
    let b = Arc::new(ConfirmationBroker::default());
    // With the setting on: kept, cut, on one line.
    crate::flags::with_overrides(&[("AQUA_DENY_FEEDBACK", Some("on"))], || {
        let (id, asker) = open_one(&b, ConfirmKind::Tool, false, Duration::from_secs(30));
        let long = format!("edit  config.toml\ninstead {}", "word ".repeat(100));
        let (code, body) = answer_confirmation_with_reason(&b, &id, "deny", Some(&long));
        assert_eq!((code, body["answer"].clone(), body["reason_given_to_the_run"].clone()), (StatusCode::OK, serde_json::json!("deny"), serde_json::json!(true)), "{body}");
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        // (The asker of this test is the broker's own thread, which does not take the words: they wait for the host's approver. They are what `take_reason` gives.)
        let said = b.take_reason(&id).expect("the words were kept for the run");
        assert!(said.starts_with("edit config.toml instead word word") && !said.contains('\n') && said.chars().count() <= aqua_tool_host::MAX_PERSON_SAID_CHARS + 1, "{said}");
        // A yes with words is refused, and the question stays open and can be answered.
        let (id, asker) = open_one(&b, ConfirmKind::Tool, false, Duration::from_secs(30));
        let (code, body) = answer_confirmation_with_reason(&b, &id, "approve", Some("and delete the rest"));
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(b.pending().len(), 1, "the question is still open");
        assert_eq!(answer_confirmation_with_reason(&b, &id, "approve", None).0, StatusCode::OK);
        assert_eq!(asker.join().unwrap(), Decision::Allow);
        // A no with blank words is a plain no.
        let (id, asker) = open_one(&b, ConfirmKind::Tool, false, Duration::from_secs(30));
        let (_, body) = answer_confirmation_with_reason(&b, &id, "deny", Some("   "));
        assert_eq!(body["answer"], "deny");
        assert!(body.get("reason_given_to_the_run").is_none(), "{body}");
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        assert_eq!(b.take_reason(&id), None);
    });
    // With the setting off (the default): the same no, and the words are not kept.
    crate::flags::with_overrides(&[("AQUA_DENY_FEEDBACK", None)], || {
        let (id, asker) = open_one(&b, ConfirmKind::Tool, false, Duration::from_secs(30));
        let (code, body) = answer_confirmation_with_reason(&b, &id, "deny", Some("edit config.toml instead"));
        assert_eq!((code, body["reason_given_to_the_run"].clone()), (StatusCode::OK, serde_json::json!(false)), "{body}");
        assert!(body["note"].as_str().unwrap().contains("AQUA_DENY_FEEDBACK"), "{body}");
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        assert_eq!(b.take_reason(&id), None, "nothing was kept");
    });
}
