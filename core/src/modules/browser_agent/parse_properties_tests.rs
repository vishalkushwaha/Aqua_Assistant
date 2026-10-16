//! Property-style and fuzz-style tests of the tolerant reader of the planner's reply (card OA13-01, task QA-06): `parse_action` and `extract_json_object`. A model that is not held to a
//! grammar writes its call with words before it, a code fence around it, a `<think>` block in front of it, and text in its arguments with every character a program has in it. The reader
//! must find the call it wrote, as it wrote it, and must never panic on what is not one.
//!
//! The case loop is `adapters/tool_host/tests/prop_kit/mod.rs`: fixed seeds and counts, `AQUA_PROP_CASES` for a longer run, a failure names its seed and its case.

#[path = "../../../../adapters/tool_host/tests/prop_kit/mod.rs"]
mod prop_kit;

use super::*;
use prop_kit::{check, mutate, Rng};

const TOOLS: &[&str] = &["fs.read", "fs.edit", "term.run", "git.status", "browser_click", "win.type"];

/// An argument value as a model writes one: text with the characters of code in it (braces that do not balance, quotes, backslashes, line breaks), numbers, flags, lists, objects.
fn value(rng: &mut Rng, depth: usize, backticks: bool) -> Value {
    match rng.below(if depth == 0 { 4 } else { 6 }) {
        0 | 1 => {
            let mut s = rng.nasty(6);
            if !backticks {
                s = s.replace("```", "'''");
            }
            if rng.chance(30) {
                s.push_str(rng.pick(&["fn main() {", "}", "{\"a\": ", "if (x) { y();", "} else {", "\\n\\\"", "{{", "}}"]));
            }
            Value::String(s)
        }
        2 => json!(rng.range(0, 100000)),
        3 => json!(rng.chance(50)),
        4 => Value::Array((0..rng.range(0, 3)).map(|_| value(rng, depth - 1, backticks)).collect()),
        _ => {
            let mut o = serde_json::Map::new();
            for _ in 0..rng.range(0, 3) {
                o.insert(rng.word(1, 6), value(rng, depth - 1, backticks));
            }
            Value::Object(o)
        }
    }
}

fn arguments(rng: &mut Rng, backticks: bool) -> Value {
    let mut o = serde_json::Map::new();
    for _ in 0..rng.range(0, 4) {
        o.insert(rng.word(1, 8), value(rng, 2, backticks));
    }
    Value::Object(o)
}

/// Words with no brace, no quote and no backtick in them.
fn prose(rng: &mut Rng) -> String {
    (0..rng.range(1, 12)).map(|_| rng.word(1, 8)).collect::<Vec<_>>().join(" ")
}

fn dump(rng: &mut Rng, v: &Value) -> String {
    if rng.chance(30) {
        serde_json::to_string_pretty(v).unwrap()
    } else {
        v.to_string()
    }
}

#[test]
fn a_call_is_found_as_the_model_wrote_it_whatever_is_written_around_it() {
    check("planner_reply_call", 800, |rng, _| {
        let tool = rng.pick(TOOLS).to_string();
        let args = arguments(rng, false);
        let object = json!({"tool": tool, "arguments": args});
        let json_text = dump(rng, &object);
        let words = prose(rng);
        let text = match rng.below(6) {
            0 => json_text.clone(),
            1 => format!("{words}\n{json_text}"),
            2 => format!("{json_text}\n{words}"),
            3 => format!("{words}: {json_text} {words}"),
            4 => format!("```json\n{json_text}\n```"),
            _ => format!("<think>{words} {{ not the call }} {{</think>\n{json_text}"),
        };
        match parse_action(&text) {
            Ok(LlmAction::Tool { tool: got_tool, arguments: got_args }) => {
                assert_eq!(got_tool, tool, "{text:?}");
                assert_eq!(got_args, args, "the arguments came back different: {text:?}");
            }
            other => panic!("a call that the model wrote was not read as a call: {other:?} from {text:?}"),
        }
    });
}

#[test]
fn a_final_answer_a_question_and_a_recall_are_found_the_same_way() {
    check("planner_reply_other", 500, |rng, _| {
        let words = prose(rng);
        let answer = format!("{} {}", rng.nasty(4), words);
        let (object, expect): (Value, &str) = match rng.below(4) {
            0 => (json!({"final": answer}), "final"),
            1 => (json!({"final_answer": answer}), "final_answer"),
            2 => (json!({"ask": format!("{words}?"), "choices": ["yes", "no"]}), "ask"),
            _ => (json!({"recall": "h3"}), "recall"),
        };
        let json_text = dump(rng, &object);
        let text = if rng.chance(50) { json_text } else { format!("{words}\n{json_text}") };
        let got = parse_action(&text).unwrap_or_else(|e| panic!("{expect} was not read ({e}): {text:?}"));
        match (expect, got) {
            ("final", LlmAction::FinalAlt { final_text }) => assert_eq!(final_text.trim(), crate::modules::llm_router::strip_reasoning_tags(&answer).trim(), "{text:?}"),
            ("final_answer", LlmAction::Final { final_answer }) => assert_eq!(final_answer.trim(), crate::modules::llm_router::strip_reasoning_tags(&answer).trim(), "{text:?}"),
            ("ask", LlmAction::Ask { question, choices }) => assert!(question.trim_end().ends_with('?') && choices == vec!["yes".to_string(), "no".to_string()], "{question:?} {choices:?}"),
            ("recall", LlmAction::Recall { handle }) => assert_eq!(handle, "h3"),
            (_, other) => panic!("{expect} was read as {other:?}: {text:?}"),
        }
    });
}

#[test]
fn text_that_holds_no_call_is_the_answer_and_nothing_makes_the_reader_panic() {
    let dictionary: &[&[u8]] = &[b"{\"tool\":", b"\"arguments\":", b"```json", b"```", b"<think>", b"</think>", b"{\"final\":", b"\\\"", b"\\u00", b"}}", b"{{", b"\xe0\xa4\xa8", b"\xf0\x9f\x8e\x89", b"\xff"];
    check("planner_reply_never_panics", 1500, |rng, _| {
        let mut bytes = match rng.below(3) {
            0 => json!({"tool": rng.pick(TOOLS), "arguments": arguments(rng, true)}).to_string().into_bytes(),
            1 => format!("{} {{\"tool\": \"fs.read\", \"arguments\": {{\"path\": \"a\"}}}} {}", prose(rng), prose(rng)).into_bytes(),
            _ => {
                let n = rng.range(0, 120);
                rng.bytes(n)
            }
        };
        mutate(rng, &mut bytes, dictionary, 6);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let _ = parse_action(&text);
        if let Some(found) = extract_json_object(&text) {
            // What is found is a JSON text, and a part of the reply.
            assert!(serde_json::from_str::<Value>(&found).is_ok(), "{found:?} from {text:?}");
        }
        let _ = words_of_unreadable_reply(&text);
        // A reply with no brace in it is read as the answer, whatever else is in it.
        if !text.contains('{') && !text.contains("```") {
            match parse_action(&text) {
                Ok(LlmAction::Final { .. }) => {}
                other => panic!("a reply with no call in it is not the model's final answer: {other:?} from {text:?}"),
            }
        }
    });
}

#[test]
fn a_call_whose_text_has_an_unbalanced_brace_in_it_is_still_found_inside_words() {
    // Found by the property test above: the reader counted the braces of the whole reply, and one in a string of the arguments (code: `fn main() {`) made it give up and take the
    // call for the model's answer.
    let call = json!({"tool": "fs.write", "arguments": {"path": "a.rs", "content": "fn main() {\n    println!(\"hi\");\n"}});
    let text = format!("I will write the file now.\n{}\nThat is all.", call);
    match parse_action(&text) {
        Ok(LlmAction::Tool { tool, arguments }) => {
            assert_eq!(tool, "fs.write");
            assert_eq!(arguments, call["arguments"]);
        }
        other => panic!("the call was not found: {other:?}"),
    }
    // A quote inside the text, escaped, does not end the string for the reader either.
    let call = json!({"tool": "term.run", "arguments": {"command": "echo \"}\" && echo '{'"}});
    let text = format!("Running it: {call} ok");
    assert!(matches!(parse_action(&text), Ok(LlmAction::Tool { .. })), "{text}");
}
