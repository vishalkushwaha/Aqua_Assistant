//! Asking the model server for an answer: the `complete_*` calls and the one request they are all cases of, which waits for its turn, starts the server when it needs one, sends the request, watches it (Stop, the time limit, a quiet stream, a person who is waiting), reads the reply and moves from the graphics card to the processor when the card fails (move only: this was part of `llama_server.rs`).

use super::*;

/// Ask the server for one planner answer, starting it if needed.
pub fn complete(model: &Path, prompt: &str, max_tokens: u32, timeout: Duration, on_tick: impl FnMut(u64)) -> Result<String, Failure> {
    complete_checked(model, prompt, max_tokens, None, timeout, on_tick).map(|r| r.text)
}

/// Like [`complete`], and says whether the answer was cut off at `max_tokens` (so the caller can ask again with more room
/// instead of treating half a JSON object as the model's answer).
pub fn complete_checked(model: &Path, prompt: &str, max_tokens: u32, slot: Option<u32>, timeout: Duration, mut on_tick: impl FnMut(u64)) -> Result<Reply, Failure> {
    complete_inner(model, prompt, max_tokens, slot, None, None, timeout, &mut on_tick, None, &Extras::default())
}

/// One decode step held to a closed set of answers (`labels`: the model can write one of them and nothing else), with the probabilities the model gave to the tokens it
/// considered for it: the answer is read from `first_token_probs`, and how sure the model was is in the numbers. No text is written beyond the one word, so a call is a read of
/// the prompt and one step.
pub fn complete_choice(model: &Path, prompt: &str, labels: &[String], slot: Option<u32>, timeout: Duration) -> Result<Reply, Failure> {
    let grammar = Constraint::Grammar(choice_grammar(labels));
    complete_inner(model, prompt, 1, slot, Some(&grammar), Some(20), timeout, &mut |_| {}, None, &Extras::default())
}

/// Like [`complete_checked`], with the reply held to `constraint` as it is written (the planner's JSON, for one).
pub fn complete_constrained(model: &Path, prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>, timeout: Duration, mut on_tick: impl FnMut(u64)) -> Result<Reply, Failure> {
    complete_inner(model, prompt, max_tokens, slot, constraint, None, timeout, &mut on_tick, None, &Extras::default())
}

/// Like [`complete_checked`], and the answer is also handed to `on_text` as it is written, a few characters at a time, so that the
/// start of a long answer can be shown and spoken while the rest is still being made (a 60-token answer takes 10 s to write on this
/// laptop). `on_text` returns `false` to stop (the person is gone): the request is hung up on and this ends with
/// [`Failure::Cancelled`]. The returned [`Reply`] has the whole text, as ever. A server that fails after some text was handed over is
/// not retried, since the retry would say it all again.
pub fn complete_streaming(
    model: &Path,
    prompt: &str,
    max_tokens: u32,
    slot: Option<u32>,
    timeout: Duration,
    mut on_tick: impl FnMut(u64),
    mut on_text: impl FnMut(&str) -> bool,
) -> Result<Reply, Failure> {
    complete_inner(model, prompt, max_tokens, slot, None, None, timeout, &mut on_tick, Some(&mut on_text), &Extras::default())
}

/// What the thread that talks to the server tells the one that waits for it.
enum Streamed {
    Text(String),
    Done(Result<Value, String>),
}

/// The one request to the server that every `complete_*` above is a case of (and the model layer's `LlamaServerProvider` the other): `extras` is what only that layer sets, and
/// `Extras::default()` changes nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn complete_inner(
    model: &Path,
    prompt: &str,
    max_tokens: u32,
    slot: Option<u32>,
    constraint: Option<&Constraint>,
    logprobs: Option<u32>,
    timeout: Duration,
    on_tick: &mut dyn FnMut(u64),
    mut on_text: Option<&mut dyn FnMut(&str) -> bool>,
    extras: &Extras<'_>,
) -> Result<Reply, Failure> {
    let ceiling = prompt_chars_ceiling();
    if prompt.len() > ceiling && prompt.chars().count() > ceiling {
        return Err(Failure::Rejected(too_long(prompt.chars().count())));
    }
    let wait = queue_wait();
    let Some(mut guard) = lock_within(&SERVER, wait, &mut *on_tick) else {
        return Err(Failure::Timeout(busy_message(wait.as_secs())));
    };
    // A server for another model, or one that died, is replaced.
    let usable = match guard.as_mut() {
        Some(r) => r.model == model && r.child.try_wait().ok().flatten().is_none(),
        None => false,
    };
    if !usable {
        if guard.as_mut().is_some_and(|r| r.gpu.is_some() && r.child.try_wait().ok().flatten().is_some()) {
            super::gpu::note_failure("the model server on the graphics card had exited");
        }
        stop(guard.take());
        *guard = Some(spawn_server(model, &mut *on_tick)?);
        crate::metrics::inc("aqua_model_loads_total", &[]);
        show(guard.as_ref());
        start_reaper();
    }
    let (port, key) = {
        let r = guard.as_ref().ok_or_else(|| Failure::Unavailable("llama-server vanished".into()))?;
        (r.port, r.key.clone())
    };
    // Native tool calls (card OA9-06): when the planner made an offer for this request (and the request is a plain one: not streamed, not a decision) the body carries the tools and not the
    // schema, and the reply is read back into the text the planner's reader has always read. Without an offer the body is `build_body` as it always was.
    let native = if on_text.is_none() && logprobs.is_none() { crate::model::native_tools::current_offer() } else { None };
    let body = body_with_offer(prompt, max_tokens, slot, constraint, logprobs, extras, native.as_deref());
    let started = Instant::now();
    let streaming = on_text.is_some();
    // How long a stream may be quiet (card OA9-02): off unless a setting or the caller says so, and only for a call that streams.
    let (idle, first_event) = if streaming { stream_watch(extras) } else { (None, None) };
    let (tx, rx) = mpsc::channel::<Streamed>();
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let abort_for_thread = abort.clone();
    let pulse = std::sync::Arc::new(Pulse::new());
    let pulse_for_thread = pulse.clone();
    let key_for_thread = key.clone();
    std::thread::spawn(move || {
        let limit = timeout + Duration::from_secs(5);
        let res = if streaming {
            post_stream_watched(port, &key_for_thread, "/v1/chat/completions", &body, limit, &abort_for_thread, &pulse_for_thread, |piece| {
                let _ = tx.send(Streamed::Text(piece.to_string()));
            })
        } else {
            post_json(port, &key_for_thread, "/v1/chat/completions", &body, limit, &abort_for_thread)
        };
        let _ = tx.send(Streamed::Done(res));
    });
    let mut last_tick = 0u64;
    let mut delivered = false;
    // A background run gives the model up to a person who is waiting for it (card R6-03, T6-03.4; off unless `AQUA_BACKGROUND_YIELD=on`). Known once: the thread does not change.
    let may_yield = background_yield_enabled() && crate::queue::in_background() && !NO_YIELD.with(|c| c.get());
    let reply = loop {
        if should_yield(may_yield, true, person_waiting()) {
            // Hang up, as for Stop: the server notices the closed connection and stops writing, and stays loaded; what it has read of the prompt stays in its cache, so the call made
            // again at the next step boundary does not read it all over. The caller is told this is not the person's Stop.
            abort.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = rx.recv_timeout(Duration::from_millis(600));
            return Err(yielded_failure());
        }
        // Checked on every turn of the loop, not only when nothing arrives: a stream of text arrives every few tenths of a second, so
        // a check that waited for a quiet moment would never run.
        if stopped(extras) {
            // Stopped by the person: hang up. The server notices the closed connection and stops writing, and
            // stays loaded, so the next task does not wait for a reload.
            abort.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = rx.recv_timeout(Duration::from_millis(600)); // the request thread notices within a quarter second
            return Err(Failure::Cancelled);
        }
        let secs = started.elapsed().as_secs();
        if started.elapsed() >= timeout {
            // Hang up. The server notices the closed connection and stops writing (as it does when a person presses Stop), so a
            // server that was only slow keeps its model and both prompt caches; the next request would otherwise wait for a
            // reload of 15 to 40 seconds and read its prompts again. One that is still busy a few seconds later, or does not
            // answer its health check, is stuck and is replaced.
            abort.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = rx.recv_timeout(Duration::from_millis(600));
            if guard.as_ref().is_some_and(|r| r.gpu.is_some()) {
                // A model on the card is several times faster than on the processor, so a request that outlasts its limit
                // there means the card is in trouble (its memory has overflowed, for one: another program started using
                // it). The next start is on the processor, which at least answers at a steady speed.
                super::gpu::note_failure("a request ran past its time limit on the graphics card");
                stop(guard.take());
            } else if !idle_and_healthy(port, &key) {
                stop(guard.take());
            }
            return Err(Failure::Timeout(format!(
                "llama timed out after {}s (model={}, prompt was {} chars). Other programs using the CPU slow this down: close heavy ones, prefer a smaller agent GGUF, or raise AQUA_LLAMA_TIMEOUT_SECS.",
                timeout.as_secs(),
                model.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                prompt.len()
            )));
        }
        // A stream that has gone quiet (card OA9-02, task PR-03): hung up on like a time-out, but the server is only asked whether it is well, and kept when it is. It is not marked as a
        // fault of the graphics card (a quiet stream is as likely the model as the card) and it is not asked again: some of the answer may already have been handed over.
        if let Some(quiet) = quiet_verdict(idle, first_event, started.elapsed(), pulse.quiet_for()) {
            let (before_first, quiet_secs) = match quiet {
                Quiet::BeforeFirst(d) => (true, d.as_secs()),
                Quiet::InTheMiddle(d) => (false, d.as_secs()),
            };
            abort.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = rx.recv_timeout(Duration::from_millis(600));
            let kept = idle_and_healthy(port, &key);
            if !kept {
                stop(guard.take());
            }
            crate::metrics::inc("aqua_model_idle_timeouts_total", &[("when", if before_first { "before_first_event" } else { "in_the_answer" })]);
            warn!("llama-server went quiet {} ({quiet_secs}s without an event); the request was hung up on", if before_first { "before it began to answer" } else { "in the middle of the answer" });
            return Err(Failure::Timeout(idle_message(before_first, quiet_secs, kept)));
        }
        if secs > last_tick {
            last_tick = secs;
            on_tick(secs);
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Streamed::Done(r)) => break r,
            Ok(Streamed::Text(piece)) => {
                if let Some(deliver) = on_text.as_mut() {
                    delivered = true;
                    if !deliver(&piece) {
                        // Whoever wanted the text is gone: hang up, as for Stop.
                        abort.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = rx.recv_timeout(Duration::from_millis(600));
                        return Err(Failure::Cancelled);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("the request thread ended unexpectedly".to_string()),
        }
    };
    let reply = match (reply, &native) {
        (Ok(v), Some(offer)) => {
            let (v, outcome) = offer.rewrite_reply(v);
            crate::model::native_tools::note_reply(outcome);
            Ok(v)
        }
        (other, _) => other,
    };
    match reply {
        Ok(v) => match parse_reply(&v) {
            Some((text, read, cached)) => {
                if let Some(r) = guard.as_mut() {
                    r.last_used = Instant::now();
                }
                let cut_off = reply_was_cut_off(&v);
                // What a draft model's guesses came to (card OA9-06): added up when the server says them, and nothing when it does not.
                super::draft_model::note_timings(&v["timings"]);
                if guard.as_ref().is_some_and(|r| r.gpu.is_some()) {
                    let timings = &v["timings"];
                    match (timings["prompt_n"].as_u64(), timings["prompt_per_second"].as_f64()) {
                        (Some(tokens), Some(speed)) if super::gpu::reading_is_too_slow(tokens, speed) => {
                            // This answer is fine, but the next ones would be as slow: the processor is faster.
                            super::gpu::note_failure(&format!("prompt reading on the graphics card fell to {speed:.0} tokens a second"));
                            stop(guard.take());
                        }
                        _ => super::gpu::note_success(),
                    }
                }
                info!(
                    "llama-server answered in {}s (prompt tokens read {read}, reused from cache {cached}){}{}",
                    started.elapsed().as_secs(),
                    timings_note(&v),
                    if cut_off { " - cut off at the token limit" } else { "" }
                );
                Ok(Reply { text, cut_off, prompt_tokens_read: read, prompt_tokens_cached: cached, timings: timings_of(&v), first_token_probs: if logprobs.is_some() { top_probs_of(&v) } else { Vec::new() } })
            }
            None => {
                stop(guard.take());
                Err(Failure::Unavailable(format!("unexpected reply from llama-server: {}", v.to_string().chars().take(200).collect::<String>())))
            }
        },
        Err(e) if is_client_rejection(&e) => {
            // The server answered and refused this request (a prompt past its context, a bad field). It is healthy:
            // stopping it would cost a 30-second reload for everyone after one bad request.
            if let Some(r) = guard.as_mut() {
                r.last_used = Instant::now();
            }
            Err(Failure::Rejected(refusal_message(&e)))
        }
        Err(e) => {
            // Anything else (a crash, a refused connection): start clean next time. On the card that is what a lost device or
            // an out-of-memory error looks like, so the next start is on the processor.
            let on_card = guard.as_ref().and_then(|r| r.gpu.clone());
            if let Some(device) = &on_card {
                super::gpu::note_failure(&format!("the server on {device} failed: {}", e.chars().take(120).collect::<String>()));
            }
            stop(guard.take());
            if on_card.is_some() && !delivered && !stopped(extras) {
                // The card is now left alone (see `gpu`), so this starts the server on the processor: the request is answered, a
                // little later, instead of falling back to `llama-cli`, which loads the model again for every call.
                drop(guard);
                info!("retrying the request on the processor");
                return complete_inner(model, prompt, max_tokens, slot, constraint, logprobs, timeout, on_tick, on_text, extras);
            }
            Err(Failure::Unavailable(anyhow!("{e}").to_string()))
        }
    }
}
