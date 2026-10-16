use super::*;

/// Card OA10-06 (UX-08): `"tts": false` in the body and `?tts=0` in the URL both ask for the words only; a request that says neither is spoken.
#[test]
fn a_request_asks_for_words_only_with_tts_false_or_tts_0() {
    assert!(wants_sound(None, None) && wants_sound(Some(true), None) && wants_sound(None, Some("1")) && wants_sound(None, Some("on")) && wants_sound(None, Some("")));
    assert!(!wants_sound(Some(false), None));
    for off in ["0", "false", "FALSE", " off ", "no"] {
        assert!(!wants_sound(None, Some(off)), "{off}");
    }
    // Either one is enough: the body can ask for words only when the URL does not say, and a body that says `true` does not override the URL.
    assert!(!wants_sound(Some(true), Some("0")), "the URL asks for words only");
    let request: PipelineTextRequest = serde_json::from_value(serde_json::json!({ "text": "hi", "tts": false })).unwrap();
    assert_eq!(request.tts, Some(false));
    let plain: PipelineTextRequest = serde_json::from_value(serde_json::json!({ "text": "hi" })).unwrap();
    assert_eq!(plain.tts, None, "not given: spoken, as it always was");
}

/// Card OA10-06 (UX-08): the token stream flattened line breaks into spaces, so code and lists lost their shape; `?format=json` sends them inside the JSON, and the default is as it was.
#[test]
fn the_json_token_stream_keeps_line_breaks_and_the_default_is_unchanged() {
    assert_eq!(token_data("fn main() {\n    x\r\n}", false), "fn main() {     x }", "the default flattens, as it always did");
    let json: serde_json::Value = serde_json::from_str(&token_data("fn main() {\n    x\r\n}", true)).unwrap();
    assert_eq!(json["t"], "fn main() {\n    x\n}");
    // A piece with a quote or a backslash is still one line of valid JSON.
    let tricky = token_data("say \"hi\" \\ done\n", true);
    assert!(!tricky.contains('\n'), "an event's data is one line: {tricky}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&tricky).unwrap()["t"], "say \"hi\" \\ done\n");
}
