//! Tests of `http_calls.rs`: the chunked decoder, the server-sent events, the streamed call against a server that is cut off or paced, the watch on a quiet stream, and the streamed answer of the real server (move only: this was the inline module `stream_tests` of `llama_server.rs` and one test of its inline module `tests`; the path of every test is now `modules::llm_router::llama_server::stream_tests::<name>`).

use super::*;

#[test]
fn a_chunked_reply_is_joined() {
    assert_eq!(dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), b"hello world");
    assert_eq!(dechunk(b"0\r\n\r\n"), b"");
}

#[test]
fn a_chunked_body_is_decoded_however_it_is_cut_up() {
    let body = b"5\r\nhello\r\n6;ext=1\r\n world\r\n1\r\n!\r\n0\r\n\r\n";
    for cut in 0..=body.len() {
        let mut decoder = ChunkedDecoder::default();
        let mut out = decoder.feed(&body[..cut]);
        out.extend(decoder.feed(&body[cut..]));
        assert_eq!(String::from_utf8_lossy(&out), "hello world!", "cut at {cut}");
    }
    let mut one_at_a_time = ChunkedDecoder::default();
    let out: Vec<u8> = body.iter().flat_map(|b| one_at_a_time.feed(&[*b])).collect();
    assert_eq!(String::from_utf8_lossy(&out), "hello world!");
}

#[test]
fn server_sent_events_are_cut_out_whole_whatever_the_pieces() {
    let stream = b"data: {\"a\":1}\n\n: keep-alive\n\ndata: {\"b\":\ndata: 2}\n\ndata: [DONE]\n\n";
    let mut splitter = EventSplitter::default();
    let events: Vec<String> = stream.iter().flat_map(|b| splitter.feed(&[*b])).collect();
    assert_eq!(events, ["{\"a\":1}", "{\"b\":\n2}", "[DONE]"], "an event may have several data lines; a comment is not an event");
    let mut whole = EventSplitter::default();
    assert_eq!(whole.feed(stream).len(), 3);
    // A half event waits for its end.
    let mut half = EventSplitter::default();
    assert!(half.feed(b"data: {\"a\"").is_empty());
    assert_eq!(half.feed(b":1}\n\n"), ["{\"a\":1}"]);
}

/// Found in a review: each read was turned into text by itself, so a character cut between two reads (a degree sign in a weather answer,
/// an accent, an emoji) came out as two replacement marks.
#[test]
fn a_character_cut_between_two_reads_stays_whole() {
    let text = "It is 24\u{b0}C in Z\u{fc}rich, \u{20b9}500 for caf\u{e9}, \u{1f600} done.";
    let stream = format!("data: {{\"c\":\"{text}\"}}\n\ndata: [DONE]\n\n").into_bytes();
    let want = vec![format!("{{\"c\":\"{text}\"}}"), "[DONE]".to_string()];
    for cut in 0..=stream.len() {
        let mut splitter = EventSplitter::default();
        let mut got = splitter.feed(&stream[..cut]);
        got.extend(splitter.feed(&stream[cut..]));
        assert_eq!(got, want, "cut at byte {cut}");
    }
    let mut one_at_a_time = EventSplitter::default();
    let got: Vec<String> = stream.iter().flat_map(|b| one_at_a_time.feed(&[*b])).collect();
    assert_eq!(got, want);
}

/// A one-shot model server: reads the request, sends `reply` as it is, closes the connection. Returns its port.
fn serve_once(reply: Vec<u8>) -> u16 {
    serve_paced(vec![(0, reply)])
}

/// A one-request model server that reads the request, then sends `parts` in order, each after its wait in milliseconds, and closes the connection. Returns its port.
fn serve_paced(parts: Vec<(u64, Vec<u8>)>) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else { return };
        let mut request: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        // The whole request first (header, then the body it announces), or closing early would reset the connection under the client.
        loop {
            let Ok(n) = conn.read(&mut buf) else { break };
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..split]).to_ascii_lowercase();
                let length: usize = head.split("content-length:").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok()).unwrap_or(0);
                if request.len() >= split + 4 + length {
                    break;
                }
            }
        }
        for (wait, bytes) in parts {
            std::thread::sleep(Duration::from_millis(wait));
            if conn.write_all(&bytes).is_err() {
                return;
            }
            let _ = conn.flush();
        }
    });
    port
}

fn sse_reply(events: &[&str], end_of_stream: bool) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for e in events {
        let data = format!("data: {e}\n\n");
        out.extend_from_slice(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes());
    }
    if end_of_stream {
        out.extend_from_slice(b"0\r\n\r\n");
    }
    out
}

fn stream_from(reply: Vec<u8>) -> (Result<Value, String>, String) {
    let port = serve_once(reply);
    let mut heard = String::new();
    let abort = std::sync::atomic::AtomicBool::new(false);
    let r = post_stream(port, "key", "/v1/chat/completions", &json!({"messages": []}), Duration::from_secs(20), &abort, |piece| heard.push_str(piece));
    (r, heard)
}

const HELLO: &str = r#"{"choices":[{"delta":{"content":"Hello "}}]}"#;
const WORLD: &str = r#"{"choices":[{"delta":{"content":"world"}}]}"#;
const STOP: &str = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"timings":{"prompt_n":3,"predicted_n":2}}"#;

/// Found in a review: a connection that ended early was taken for the end of the answer, so the start of an answer was shown, spoken
/// and remembered as the whole of it (a server killed by the system's out-of-memory handling is how this happens in real life).
#[test]
fn a_stream_that_ends_before_the_end_of_the_answer_is_an_error_and_not_a_short_answer() {
    let (cut_in_the_middle, heard) = stream_from(sse_reply(&[HELLO], false));
    let error = cut_in_the_middle.unwrap_err();
    assert!(error.contains("stopped in the middle of the answer") && error.contains("6 characters"), "{error}");
    assert_eq!(heard, "Hello ", "what came was handed over as it came; the caller decides what to do with an error");
    let (nothing_at_all, _) = stream_from(sse_reply(&[], false));
    assert!(nothing_at_all.unwrap_err().contains("stopped in the middle"));
    // The end of the chunked body without the closing event is the same.
    let (no_done, _) = stream_from(sse_reply(&[HELLO, WORLD], true));
    assert!(no_done.is_err(), "no finish reason and no [DONE]");
}

#[test]
fn a_stream_that_ends_properly_is_the_whole_answer() {
    let (with_done, heard) = stream_from(sse_reply(&[HELLO, WORLD, STOP, "[DONE]"], true));
    let reply = with_done.unwrap();
    assert_eq!(heard, "Hello world");
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello world");
    assert_eq!(reply["choices"][0]["finish_reason"], "stop");
    assert_eq!(reply["timings"]["predicted_n"], 2);
    // A finish reason is enough: a connection closed just before [DONE] is not an answer cut short.
    let (finished_without_done, _) = stream_from(sse_reply(&[HELLO, STOP], false));
    assert_eq!(finished_without_done.unwrap()["choices"][0]["message"]["content"], "Hello ");
    // An answer of no words that ends properly is an empty answer, which the caller already handles.
    let (empty, _) = stream_from(sse_reply(&[STOP, "[DONE]"], true));
    assert_eq!(empty.unwrap()["choices"][0]["message"]["content"], "");
}

/// Against the real server (the small model): the answer arrives in several pieces that add up to the reply, and the reply is the
/// same shape as one that was not streamed (the text and the speeds). Hanging up on the stream stops it and keeps the server.
#[test]
fn the_real_server_streams_the_answer_in_pieces_and_stops_when_asked() {
    let _only_one = super::tests::THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    shutdown();
    let mut pieces: Vec<String> = Vec::new();
    let reply = complete_streaming(&model, "Count from one to twelve in words, separated by commas.", 80, None, Duration::from_secs(300), |_| {}, |piece| {
        pieces.push(piece.to_string());
        true
    })
    .unwrap_or_else(|e| panic!("streamed request: {e:?}"));
    assert!(pieces.len() >= 5, "several pieces, not one: {pieces:?}");
    assert_eq!(pieces.concat(), reply.text, "the pieces add up to the reply");
    assert!(reply.prompt_tokens_read > 0, "the speeds of the last event were kept: {reply:?}");
    // The same question, not streamed.
    let whole = complete_checked(&model, "Count from one to twelve in words, separated by commas.", 80, None, Duration::from_secs(300), |_| {}).unwrap();
    assert!(!whole.text.trim().is_empty());
    // Hang up after a few pieces.
    let mut seen = 0;
    let stopped = complete_streaming(&model, "Write a very long story about a dragon, with many chapters.", 400, None, Duration::from_secs(300), |_| {}, |_| {
        seen += 1;
        seen < 4
    });
    assert!(matches!(stopped, Err(Failure::Cancelled)), "{stopped:?}");
    assert!(is_running(), "hanging up keeps the server loaded");
    assert!(complete(&model, "Say hi.", 4, Duration::from_secs(300), |_| {}).is_ok(), "and it answers the next request");
    shutdown();
}

/// The chat call built on it, against the real server (the small model): the pieces add up to the answer it returns, which is the
/// cleaned text a whole-answer call gives, and a listener that goes away ends the call with "stopped" and leaves the server loaded.
#[test]
fn the_chat_call_hands_over_its_answer_in_pieces_and_ends_when_the_listener_leaves() {
    let _only_one = super::tests::THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm");
    let small = "qwen-0.5b-instruct-q4_k_m.gguf";
    if server_binary().is_none() || !model_dir.join(small).is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    shutdown();
    let engine = crate::modules::llm_router::LlamaRouterEngine::new(&model_dir).unwrap();
    let mut pieces: Vec<String> = Vec::new();
    let chat = engine
        .generate_chat_streaming_with_model("Count from one to twelve in words, separated by commas.", small, |piece| {
            pieces.push(piece.to_string());
            true
        })
        .unwrap_or_else(|e| panic!("streamed chat: {e:#}"));
    assert!(pieces.len() >= 5, "several pieces, not one: {pieces:?}");
    assert!(!chat.cut_off && !chat.text.is_empty(), "{chat:?}");
    assert_eq!(crate::modules::llm_router::strip_reasoning_tags(pieces.concat().trim()), chat.text, "the pieces add up to the answer");
    // A listener that leaves: the call ends as a stopped task, and the model stays loaded for the next request.
    let mut seen = 0;
    let stopped = engine.generate_chat_streaming_with_model("Write a very long story about a dragon, with many chapters.", small, |_| {
        seen += 1;
        seen < 3
    });
    let message = stopped.expect_err("the listener left").to_string();
    assert!(message.starts_with("stopped"), "{message}");
    assert!(is_running(), "hanging up keeps the server loaded");
    // Nothing to say is nothing: no request is made.
    assert_eq!(engine.generate_chat_streaming_with_model("   ", small, |_| true).unwrap(), crate::modules::llm_router::StreamedChat::default());
    shutdown();
}

// ---- the watchdog of a quiet stream (card OA9-02, task PR-03) ----------------------------------------------------------------------------------------------------

fn sse_head() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
}

fn sse_chunk(event: &str) -> Vec<u8> {
    let data = format!("data: {event}\n\n");
    format!("{:x}\r\n{data}\r\n", data.len()).into_bytes()
}

#[test]
fn a_stream_is_quiet_for_too_long_only_after_its_first_event_and_never_while_the_prompt_is_read() {
    let s = Duration::from_secs;
    let (idle, first) = (Some(s(20)), Some(s(60)));
    // Reading the prompt: no event yet. The idle limit is not armed, however long it takes; only the limit on the first event counts.
    assert_eq!(quiet_verdict(idle, None, s(45), None), None);
    assert_eq!(quiet_verdict(idle, first, s(59), None), None);
    assert_eq!(quiet_verdict(idle, first, s(60), None), Some(Quiet::BeforeFirst(s(60))));
    // The answer has begun: the idle limit counts from the last event, and the limit on the first event is over.
    assert_eq!(quiet_verdict(idle, first, s(100), Some(s(19))), None);
    assert_eq!(quiet_verdict(idle, first, s(100), Some(s(20))), Some(Quiet::InTheMiddle(s(20))));
    assert_eq!(quiet_verdict(None, first, s(100), Some(s(500))), None, "no idle limit is set");
    assert_eq!(quiet_verdict(None, None, s(1000), None), None, "no limits at all");
    assert_eq!(quiet_verdict(None, None, s(1000), Some(s(1000))), None);
}

#[test]
fn the_silence_limits_come_from_the_call_or_the_settings_and_are_off_unless_one_is_above_zero() {
    let secs = Duration::from_secs;
    let none = Extras::default();
    crate::flags::with_overrides(&[("AQUA_STREAM_IDLE_SECS", None), ("AQUA_STREAM_FIRST_EVENT_SECS", None)], || assert_eq!(stream_watch(&none), (None, None), "off by default"));
    crate::flags::with_overrides(&[("AQUA_STREAM_IDLE_SECS", Some("20")), ("AQUA_STREAM_FIRST_EVENT_SECS", Some("90"))], || {
        assert_eq!(stream_watch(&none), (Some(secs(20)), Some(secs(90))));
        // What the call was given wins over the settings.
        let own = Extras { idle: Some(secs(5)), first_event: Some(secs(7)), ..Extras::default() };
        assert_eq!(stream_watch(&own), (Some(secs(5)), Some(secs(7))));
    });
    // Zero, a word, a negative number and one past the bound are off.
    for bad in ["0", "soon", "-3", "99999", ""] {
        crate::flags::with_overrides(&[("AQUA_STREAM_IDLE_SECS", Some(bad)), ("AQUA_STREAM_FIRST_EVENT_SECS", Some(bad))], || assert_eq!(stream_watch(&none), (None, None), "{bad:?}"));
    }
}

/// Every `data:` event of a stream is noted, a piece of thinking as much as a piece of the answer: a model that thinks for a while is not one that stopped. The headers alone
/// note nothing, because the server sends them before it has read the prompt.
#[test]
fn every_event_of_a_stream_is_noted_the_thinking_ones_too_and_the_headers_alone_note_nothing() {
    const ROLE: &str = r#"{"choices":[{"delta":{"role":"assistant","content":null}}]}"#;
    const THINK: &str = r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#;
    // 300 ms to read the prompt, then twelve pieces of thinking 100 ms apart (1.2 s in all), then the answer.
    let mut parts: Vec<(u64, Vec<u8>)> = vec![(300, [sse_head(), sse_chunk(ROLE)].concat())];
    parts.extend((0..12).map(|_| (100, sse_chunk(THINK))));
    parts.extend([(100, sse_chunk(HELLO)), (0, sse_chunk(STOP)), (0, sse_chunk("[DONE]")), (0, b"0\r\n\r\n".to_vec())]);
    let port = serve_paced(parts);
    let pulse = std::sync::Arc::new(Pulse::new());
    let for_thread = pulse.clone();
    let reader = std::thread::spawn(move || {
        let abort = std::sync::atomic::AtomicBool::new(false);
        let mut heard = String::new();
        let result = post_stream_watched(port, "key", "/v1/chat/completions", &json!({"messages": []}), Duration::from_secs(30), &abort, &for_thread, |piece| heard.push_str(piece));
        (result, heard)
    });
    let before_anything = pulse.quiet_for();
    let mut longest = Duration::ZERO;
    while !reader.is_finished() {
        longest = longest.max(pulse.quiet_for().unwrap_or_default());
        std::thread::sleep(Duration::from_millis(20));
    }
    let (result, heard) = reader.join().unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(heard, "Hello ");
    assert!(before_anything.is_none(), "nothing has come yet, so nothing is armed: {before_anything:?}");
    assert!(longest < Duration::from_millis(800), "the thinking events were noted: without them the stream would look quiet for the whole 1.2 s of them, and it never looked quiet for more than {longest:?}");
    assert!(pulse.quiet_for().is_some(), "the first event armed it");

    // The headers and then nothing: the connection ends, and nothing was noted.
    let port = serve_paced(vec![(0, sse_head()), (400, b"0\r\n\r\n".to_vec())]);
    let pulse = Pulse::new();
    let abort = std::sync::atomic::AtomicBool::new(false);
    let result = post_stream_watched(port, "key", "/v1/chat/completions", &json!({"messages": []}), Duration::from_secs(20), &abort, &pulse, |_| {});
    assert!(result.unwrap_err().contains("stopped in the middle"), "an answer with no event is not an answer");
    assert!(pulse.quiet_for().is_none(), "the headers alone did not arm the watchdog");
}
