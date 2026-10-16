//! The answer made into speech, one piece at a time: the queue, the one speech worker, the order the pieces reach the listener in, and the sound of a turn's result (move only: this was part of `pipeline.rs`).

use super::*;

/// How many speech pieces may wait for the one Kokoro worker. The piece being synthesized is not in this queue.
const TTS_QUEUE_CAP: usize = 2;

static NEXT_SPEECH_TURN: AtomicU64 = AtomicU64::new(1);

struct SpeechJob {
    turn: u64,
    seq: u64,
    text: String,
    enqueued: Instant,
}

struct SpeechOut {
    turn: u64,
    seq: u64,
    text: String,
    wav: Vec<u8>,
    /// How long the piece sat in the queue before synthesis started.
    queue_ms: u64,
    synth_ms: u64,
    failed: bool,
}

/// Speaks an answer one piece at a time. Pieces are queued for a single worker, so the thread that is reading tokens does not
/// wait for Kokoro. The worker emits WAV events in order through the turn thread.
pub(super) struct Speaker<'a> {
    listener: Listener<'a>,
    splitter: crate::speech_stream::SentenceSplitter,
    pub(super) pieces: usize,
    pub(super) sample_count: usize,
    sample_rate: u32,
    /// Milliseconds spent making speech, so that the stage that was running meanwhile does not count them again.
    pub(super) tts_ms: u64,
    /// Pieces in a row whose speech could not be made. After two the rest of the turn is words only.
    failures: u32,
    /// The listener has left, or the turn was stopped.
    pub(super) gone: bool,
    turn: u64,
    next_seq: u64,
    /// Next sequence number that may be given to the listener.
    emit_seq: u64,
    pub(super) submitted: u64,
    pub(super) completed: u64,
    pub(super) dropped: u64,
    /// Pieces that did not fit in the queue, held so they are still emitted in order, without sound.
    skipped: std::collections::BTreeMap<u64, String>,
    jobs: SyncSender<SpeechJob>,
    outs: Receiver<SpeechOut>,
    stop: Arc<AtomicBool>,
    pub(super) worker: Option<std::thread::JoinHandle<()>>,
}

impl<'a> Speaker<'a> {
    pub(super) fn new(tts: &'a KokoroTtsEngine, voice: &'a str, lang: &'a str, listener: Listener<'a>) -> Self {
        Self::build(tts, voice, lang, listener, true)
    }

    /// A speaker that makes no sound (card OA10-06, UX-08: `tts: false`): the answer is cut into the same pieces and each arrives as a `Speech` event with its words and no sound, in order,
    /// as the pieces of a turn whose speech engine failed twice do. No worker is started, so no speech engine is asked and nothing waits for one.
    pub(super) fn text_only(tts: &'a KokoroTtsEngine, voice: &'a str, lang: &'a str, listener: Listener<'a>) -> Self {
        Self::build(tts, voice, lang, listener, false)
    }

    fn build(tts: &'a KokoroTtsEngine, voice: &'a str, lang: &'a str, listener: Listener<'a>, sound: bool) -> Self {
        let (jobs_tx, jobs_rx) = sync_channel::<SpeechJob>(TTS_QUEUE_CAP);
        let (out_tx, outs) = sync_channel::<SpeechOut>(TTS_QUEUE_CAP + 1);
        let stop = Arc::new(AtomicBool::new(false));
        let turn_cancel = crate::modules::browser_agent::turn_cancel_flag();
        let model_dir = tts.model_path().parent().unwrap_or(tts.model_path()).to_path_buf();
        let voice = voice.to_string();
        let lang = lang.to_string();
        let stop_for_worker = stop.clone();
        let worker = if sound {
            std::thread::Builder::new().name("aqua-tts".into()).spawn(move || {
                tts_worker(jobs_rx, out_tx, stop_for_worker, turn_cancel, model_dir, voice, lang);
            }).ok()
        } else {
            None
        };
        Self {
            listener,
            // Whether code is left out of what is said (`AQUA_CODE_SPEECH`, card OA10-06): the splitter of a turn that does not speak code drops fenced blocks and diffs and says a cue once.
            splitter: if crate::coding_voice::code_speech_on() { crate::speech_stream::SentenceSplitter::code_aware() } else { Default::default() },
            pieces: 0,
            sample_count: 0,
            sample_rate: tts.sample_rate(),
            tts_ms: 0,
            // Two failures in a row make the rest of a turn words only: a speaker that makes no sound starts there.
            failures: if sound { 0 } else { 2 },
            gone: false,
            turn: NEXT_SPEECH_TURN.fetch_add(1, Ordering::Relaxed),
            next_seq: 0,
            emit_seq: 0,
            submitted: 0,
            completed: 0,
            dropped: 0,
            skipped: std::collections::BTreeMap::new(),
            jobs: jobs_tx,
            outs,
            stop,
            worker,
        }
    }

    /// Tell the listener what was heard.
    pub(super) fn heard(&mut self, text: &str) {
        if !self.gone && !(self.listener)(TurnEvent::Heard(text.to_string())) {
            self.gone = true;
        }
    }

    /// More of the answer has been written. `false` when the listener has gone, or the turn was stopped: stop writing.
    /// Queues speech and returns. It does not wait for Kokoro.
    pub(super) fn push(&mut self, delta: &str) -> bool {
        if self.gone || self.stopped() {
            self.gone = true;
            self.stop.store(true, Ordering::SeqCst);
            return false;
        }
        self.pump();
        for piece in self.splitter.push(delta) {
            self.enqueue(piece);
            if self.gone {
                return false;
            }
        }
        self.pump();
        !self.gone
    }

    /// The answer is complete. `cut_off`: the model ran out of tokens in the middle of a sentence, which is then left out.
    /// Waits until queued speech has been delivered, unless the turn was stopped.
    pub(super) fn finish(&mut self, cut_off: bool) -> bool {
        if !self.gone && !self.stopped() {
            if let Some(piece) = self.splitter.finish(cut_off) {
                self.enqueue(piece);
            }
        } else {
            self.gone = true;
            self.stop.store(true, Ordering::SeqCst);
        }
        self.drain();
        !self.gone
    }

    /// An answer that was not written piece by piece (the agent's, a local one), spoken in pieces, the first one at once.
    fn say_all(&mut self, text: &str) -> bool {
        self.push(text) && self.finish(false)
    }

    /// The end of a turn whose answer is `text`: what has not been spoken yet is spoken now (an answer that was written piece by piece
    /// has been). An error when the listener left, because nobody heard the end.
    pub(super) fn finish_turn(&mut self, text: &str) -> Result<()> {
        if self.pieces == 0 && !self.gone {
            self.say_all(text);
        }
        if self.gone {
            anyhow::bail!("stopped: the task was cancelled");
        }
        Ok(())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || crate::modules::browser_agent::turn_cancelled()
    }

    pub(super) fn enqueue(&mut self, piece: String) {
        if self.gone || self.stopped() {
            self.gone = true;
            self.stop.store(true, Ordering::SeqCst);
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pieces += 1;
        if self.failures >= 2 {
            self.skipped.insert(seq, piece);
            self.flush_ready();
            return;
        }
        let job = SpeechJob { turn: self.turn, seq, text: piece, enqueued: Instant::now() };
        match self.jobs.try_send(job) {
            Ok(()) => {
                self.submitted += 1;
                debug!(turn = self.turn, seq, depth = self.submitted - self.completed, "tts queued");
            }
            Err(TrySendError::Full(job)) => {
                self.dropped += 1;
                info!(turn = self.turn, seq = job.seq, dropped = self.dropped, "tts queue full; this piece will be spoken without sound");
                self.skipped.insert(job.seq, job.text);
                self.flush_ready();
            }
            Err(TrySendError::Disconnected(job)) => {
                self.skipped.insert(job.seq, job.text);
                self.flush_ready();
            }
        }
    }

    /// Deliver any WAV that is already finished. Does not wait.
    fn pump(&mut self) {
        loop {
            match self.outs.try_recv() {
                Ok(out) => self.accept(out),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
            if self.gone {
                break;
            }
        }
    }

    /// Wait until every queued piece has been delivered, or the turn has been stopped.
    fn drain(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(180);
        while self.completed < self.submitted && Instant::now() < deadline {
            if self.stopped() {
                self.gone = true;
                self.stop.store(true, Ordering::SeqCst);
            }
            match self.outs.recv_timeout(Duration::from_millis(50)) {
                Ok(out) => self.accept(out),
                Err(_) => {
                    if self.stopped() && self.outs.try_recv().is_err() {
                        // The worker drops the rest without sending once stop is set and the in-flight call returns.
                        break;
                    }
                }
            }
        }
        self.pump();
        if self.stopped() {
            self.gone = true;
            self.skipped.clear();
        } else {
            self.flush_ready();
        }
    }

    fn accept(&mut self, out: SpeechOut) {
        if out.turn != self.turn {
            return;
        }
        self.completed += 1;
        if self.gone || self.stopped() {
            self.gone = true;
            self.stop.store(true, Ordering::SeqCst);
            return;
        }
        self.tts_ms += out.synth_ms;
        crate::turn_timing::note("tts", out.synth_ms);
        if out.seq == 0 {
            crate::turn_timing::note("first_tts", out.synth_ms);
        }
        crate::turn_timing::note("tts_queue", out.queue_ms);
        if out.failed {
            self.failures += 1;
            warn!("speech for one piece of the answer could not be made; its words are sent without sound");
        } else {
            self.failures = 0;
        }
        self.emit(out.seq, out.text, out.wav);
    }

    fn flush_ready(&mut self) {
        while let Some(text) = self.skipped.remove(&self.emit_seq) {
            self.emit(self.emit_seq, text, Vec::new());
            if self.gone {
                break;
            }
        }
    }

    fn emit(&mut self, seq: u64, text: String, wav: Vec<u8>) {
        if seq != self.emit_seq || self.gone {
            if seq != self.emit_seq {
                self.skipped.insert(seq, text);
            }
            return;
        }
        self.emit_seq += 1;
        if !wav.is_empty() {
            if let Ok((samples, rate)) = aqua_audio_adapter::kokoro_cli::wav_bytes_to_f32_mono(&wav) {
                self.sample_count += samples.len();
                self.sample_rate = rate;
            }
        }
        let started = Instant::now();
        if !(self.listener)(TurnEvent::Speech { text, wav }) {
            self.gone = true;
            self.stop.store(true, Ordering::SeqCst);
        }
        debug!(turn = self.turn, seq, emit_ms = started.elapsed().as_millis() as u64, "tts emitted");
        self.flush_ready();
    }
}

impl Drop for Speaker<'_> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() {
                let _ = self.outs.try_recv();
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = worker.join();
        }
    }
}

fn tts_worker(
    jobs: Receiver<SpeechJob>,
    outs: SyncSender<SpeechOut>,
    stop: Arc<AtomicBool>,
    turn_cancel: Option<Arc<AtomicBool>>,
    model_dir: PathBuf,
    voice: String,
    lang: String,
) {
    let cancelled = || stop.load(Ordering::SeqCst) || turn_cancel.as_ref().is_some_and(|f| f.load(Ordering::SeqCst));
    loop {
        let job = match jobs.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => job,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if cancelled() => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if cancelled() {
            continue;
        }
        #[cfg(test)]
        wait_for_tts_test_gate(&stop);
        if cancelled() {
            continue;
        }
        let queue_ms = job.enqueued.elapsed().as_millis() as u64;
        let started = Instant::now();
        let made = synthesize_piece(&model_dir, &job.text, &voice, &lang);
        let synth_ms = started.elapsed().as_millis() as u64;
        if cancelled() {
            continue;
        }
        let (wav, failed) = match made {
            Ok(bytes) => (bytes, false),
            Err(_) => (Vec::new(), true),
        };
        let seq = job.seq;
        let mut pending = Some(SpeechOut { turn: job.turn, seq, text: job.text, wav, queue_ms, synth_ms, failed });
        for _ in 0..100 {
            if cancelled() {
                pending = None;
                break;
            }
            let Some(out) = pending.take() else { break };
            match outs.try_send(out) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    pending = Some(returned);
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        if pending.is_some() && !cancelled() {
            debug!(seq, "tts result dropped because the turn stopped reading");
        }
    }
}

fn synthesize_piece(model_dir: &Path, text: &str, voice: &str, lang: &str) -> Result<Vec<u8>> {
    use aqua_audio_adapter::kokoro_cli::{resolve_cli, resolve_model_file, resolve_voices_file, synthesize_to_wav};
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let Some(model) = resolve_model_file(model_dir) else { return Ok(Vec::new()) };
    let Some(voices) = resolve_voices_file(model_dir) else { return Ok(Vec::new()) };
    let Some(cli) = resolve_cli() else { return Ok(Vec::new()) };
    synthesize_to_wav(&cli, &model, &voices, text, voice, lang)
}

#[cfg(test)]
fn wait_for_tts_test_gate(stop: &AtomicBool) {
    let gate = TTS_TEST_GATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(gate) = gate {
        TTS_TEST_HELD.fetch_add(1, Ordering::SeqCst);
        let (lock, cv) = &*gate;
        let mut open = lock.lock().unwrap_or_else(|e| e.into_inner());
        while !*open && !stop.load(Ordering::SeqCst) {
            let (next, _) = cv.wait_timeout(open, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner());
            open = next;
        }
    }
}

#[cfg(test)]
pub(super) static TTS_TEST_HELD: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) static TTS_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(super) static TTS_TEST_GATE: Mutex<Option<Arc<(Mutex<bool>, std::sync::Condvar)>>> = Mutex::new(None);

/// The sound part of a turn's result: for a streamed turn nothing, because it went to the listener piece by piece (the rest of the
/// answer is spoken first if no piece was); for the others the whole answer made into one WAV file. Returns the file as base64, the
/// number of samples and their rate.
pub(super) fn speech_for_result(tts: &KokoroTtsEngine, text: &str, voice: &str, lang: &str, speaker: Option<&mut Speaker<'_>>) -> Result<(String, usize, u32)> {
    match speaker {
        Some(speaker) => {
            speaker.finish_turn(text)?;
            Ok((String::new(), speaker.sample_count, speaker.sample_rate))
        }
        None => {
            // The words as they are said (an address as its site, degrees spelled out, no markdown marks); the answer shown is the same text as it was. With `AQUA_CODE_SPEECH` on, code is
            // left out of them and "The code is on the screen." is said once in its place (card OA10-06).
            let spoken = if crate::coding_voice::code_speech_on() { crate::speech_stream::for_speech_code_aware(text) } else { crate::speech_stream::for_speech(text) };
            let wav = crate::turn_timing::stage("tts", || tts.synthesize_wav(&spoken, voice, lang))?;
            let (pcm, rate) = if wav.is_empty() {
                (tts.synthesize_with_voice(&spoken, voice, lang)?, tts.sample_rate())
            } else {
                aqua_audio_adapter::kokoro_cli::wav_bytes_to_f32_mono(&wav)?
            };
            Ok((base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wav), pcm.len(), rate))
        }
    }
}
