//! Live output of a running call (card OA7-04, task TM-14 of the OpenCode audit).
//!
//! A build that runs for two minutes used to show nothing until it ended. A provider that has output while it works (the terminal tools: `term.run`, `term.start`) is given a
//! [`ProgressSink`] in `ExecCtx::progress` and tells it each line as it arrives. The sink does the rest, so no provider has to know who listens:
//!
//! * **Coalesced.** The lines of a quarter of a second ([`MIN_INTERVAL`]) leave as one piece, and a piece holds at most [`MAX_PIECE_BYTES`]: when the output is faster than that, the oldest
//!   lines of the piece are dropped and the piece says how many (`skipped`). A program that prints a million lines cannot make the listener fall behind, and costs the one that reads its
//!   pipe a lock and a push for each line.
//! * **Cleaned like a result.** Before a piece leaves the host it goes through the same steps as the text of a result (`output::sanitize`, then the host's [`Redactor`]), and a private key
//!   block that begins in one piece is dropped to its end, however the lines fall between pieces. Redaction comes before any cut, so that a secret that straddles a cut cannot survive as
//!   a prefix.
//! * **Not the model's.** A piece goes to [`ProgressSource`]'s listener (the app's event stream) and nowhere else: the model still reads only the digest the call ends with, and the audit
//!   log keeps none of it. The host makes no sink for the voice, for a call outside a run, or when nobody installed a source.
//! * **Ends with the call.** The host finishes the sink when the call ends (the last lines are flushed), unless the provider kept it for a job that goes on ([`ProgressSink::detach`]).
//!
//! The listener must not block: a piece is handed to it under the sink's lock, in order.

use crate::output::{sanitize, Redactor};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The shortest time between two pieces of one call.
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);
/// The most text a piece holds (before it is cleaned).
pub const MAX_PIECE_BYTES: usize = 8 * 1024;
/// A line of a piece is shown cut to this many characters.
pub const MAX_LINE_CHARS: usize = 1_000;
/// A line is cut to this many bytes the moment it is told, before anything is done to it: the host never works on a line of megabytes.
const RAW_LINE_BYTES: usize = 16 * 1024;
/// What a cut of a very long line takes off its end again, at a place where a word ends, so that a secret that the cut went through is not left as a beginning.
const CUT_BACK_BYTES: usize = 256;

/// One piece of live output, as the host hands it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressPiece {
    /// The number of the piece inside its call: 1, 2, 3 ...
    pub piece: u64,
    /// The lines of the piece, cleaned, joined with line breaks.
    pub text: String,
    /// How many lines were dropped before this piece because the output came faster than a piece can hold (0 most of the time).
    pub skipped: u64,
}

/// Where the pieces of one call go (made by the [`ProgressSource`] for that call).
pub type ProgressEmit = Arc<dyn Fn(ProgressPiece) + Send + Sync>;

/// What the source is told about a call that is about to run.
#[derive(Debug, Clone, Copy)]
pub struct ProgressCall<'a> {
    pub run_id: &'a str,
    pub request_id: &'a str,
    pub tool: &'a str,
}

/// Installed in the host by the program that shows output (`ToolHost::set_progress_source`): asked once for each call of a run, on the thread that made the call, and answers with where
/// its pieces go, or `None` when nobody is shown them (the call is not recorded, nobody listens).
pub type ProgressSource = Arc<dyn Fn(&ProgressCall) -> Option<ProgressEmit> + Send + Sync>;

/// What a provider tells as its program prints. Cheap to clone: every clone is the same sink.
#[derive(Clone)]
pub struct ProgressSink(Arc<Inner>);

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink")
    }
}

struct Inner {
    emit: ProgressEmit,
    interval: Duration,
    max_bytes: usize,
    detached: AtomicBool,
    state: Mutex<State>,
}

struct State {
    /// The lines told since the last piece, as they were told (cut at [`RAW_LINE_BYTES`]).
    pending: VecDeque<String>,
    pending_bytes: usize,
    skipped: u64,
    last_piece: Option<Instant>,
    piece: u64,
    closed: bool,
    cleaner: Cleaner,
}

impl ProgressSink {
    /// A sink that hands its pieces to `emit`, cleaning them with `redactor`. [`MIN_INTERVAL`] and [`MAX_PIECE_BYTES`] apply.
    pub fn new(emit: ProgressEmit, redactor: Redactor) -> Self {
        Self::with_limits(emit, redactor, MIN_INTERVAL, MAX_PIECE_BYTES)
    }

    /// [`new`](Self::new) with the time between pieces and the size of a piece given (a test uses small ones).
    pub fn with_limits(emit: ProgressEmit, redactor: Redactor, interval: Duration, max_bytes: usize) -> Self {
        let state = State { pending: VecDeque::new(), pending_bytes: 0, skipped: 0, last_piece: None, piece: 0, closed: false, cleaner: Cleaner::new(redactor) };
        Self(Arc::new(Inner { emit, interval, max_bytes: max_bytes.max(64), detached: AtomicBool::new(false), state: Mutex::new(state) }))
    }

    /// One line the program printed, without its line end. When a quarter of a second has passed since the last piece (or there was none), the piece leaves with it.
    pub fn line(&self, text: &str) {
        let mut st = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.closed {
            return;
        }
        let line = bounded(text);
        st.pending_bytes += line.len() + 1;
        st.pending.push_back(line);
        // The oldest lines make room: the newest line always stays, so that what is shown is what the program is doing now.
        while st.pending_bytes > self.0.max_bytes && st.pending.len() > 1 {
            if let Some(old) = st.pending.pop_front() {
                st.pending_bytes -= old.len() + 1;
                st.skipped += 1;
            }
        }
        if st.last_piece.is_none_or(|t| t.elapsed() >= self.0.interval) {
            self.flush(&mut st);
        }
    }

    /// Lets the lines that are waiting leave if a quarter of a second has passed since the last piece. A provider that streams calls it while it waits for its program (every few tens of
    /// milliseconds is plenty), so that the last lines of a burst are not held until the next line.
    pub fn tick(&self) {
        let mut st = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !st.closed && !st.pending.is_empty() && st.last_piece.is_none_or(|t| t.elapsed() >= self.0.interval) {
            self.flush(&mut st);
        }
    }

    /// What waits leaves now, whatever the time since the last piece. The sink stays open: a call that runs one program after another flushes at the end of each.
    pub fn flush_now(&self) {
        let mut st = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !st.closed {
            self.flush(&mut st);
        }
    }

    /// The program has ended: what waits leaves now, and the sink takes no more lines.
    pub fn finish(&self) {
        let mut st = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.closed {
            return;
        }
        self.flush(&mut st);
        st.closed = true;
    }

    /// The program goes on after the call that started it has ended (a background job): the host leaves the sink open when the call ends, and the provider finishes it when the program does.
    pub fn detach(&self) {
        self.0.detached.store(true, Ordering::SeqCst);
    }

    /// Whether the provider kept the sink for a job (see [`detach`](Self::detach)).
    pub fn is_detached(&self) -> bool {
        self.0.detached.load(Ordering::SeqCst)
    }

    /// How many pieces have left so far.
    pub fn pieces(&self) -> u64 {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner).piece
    }

    fn flush(&self, st: &mut State) {
        if st.pending.is_empty() {
            return;
        }
        let lines: Vec<String> = st.pending.drain(..).collect();
        st.pending_bytes = 0;
        let skipped = std::mem::take(&mut st.skipped);
        st.last_piece = Some(Instant::now());
        let text = st.cleaner.clean(&lines);
        if text.is_empty() && skipped == 0 {
            return;
        }
        st.piece += 1;
        (self.0.emit)(ProgressPiece { piece: st.piece, text, skipped });
    }
}

/// A line as it is kept until its piece leaves: at most [`RAW_LINE_BYTES`], cut at a character, and when it was cut, back to the last place a word ends within [`CUT_BACK_BYTES`] of the
/// cut (a secret that the cut went through is then not a beginning at the end of the line; where there is no such place the end is cut back by that much all the same).
fn bounded(text: &str) -> String {
    if text.len() <= RAW_LINE_BYTES {
        return text.to_string();
    }
    let mut end = RAW_LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let mut back = end.saturating_sub(CUT_BACK_BYTES);
    while !text.is_char_boundary(back) {
        back += 1;
    }
    let keep = match head[back..].rfind(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | '&' | '=' | ':' | '/')) {
        Some(at) => back + at,
        None => back,
    };
    format!("{}… [line cut]", &text[..keep])
}

/// Turns the lines of a piece into the text that leaves the host.
struct Cleaner {
    redactor: Redactor,
    /// Inside a private key block that began in an earlier line: everything up to its end is dropped.
    in_key: bool,
}

const KEY_DROPPED: &str = "[REDACTED private key block]";

impl Cleaner {
    fn new(redactor: Redactor) -> Self {
        Self { redactor, in_key: false }
    }

    fn clean(&mut self, lines: &[String]) -> String {
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        for raw in lines {
            let line = sanitize(raw);
            let upper = line.to_ascii_uppercase();
            let ends = upper.contains("-----END") && upper.contains("PRIVATE KEY");
            if self.in_key {
                if ends {
                    self.in_key = false;
                }
                continue;
            }
            if upper.contains("-----BEGIN") && upper.contains("PRIVATE KEY") {
                // A whole block on one line is already gone; one that goes on over the next lines is dropped to its end.
                self.in_key = !ends;
                out.push(KEY_DROPPED.to_string());
                continue;
            }
            // Masked first, cut after: a cut must not leave the beginning of a secret.
            let masked = self.redactor.redact(&line);
            if masked.chars().count() > MAX_LINE_CHARS {
                out.push(format!("{}… [line cut]", masked.chars().take(MAX_LINE_CHARS).collect::<String>()));
            } else {
                out.push(masked);
            }
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(redactor: Redactor, interval: Duration, max_bytes: usize) -> (ProgressSink, Arc<Mutex<Vec<ProgressPiece>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = seen.clone();
        let sink = ProgressSink::with_limits(Arc::new(move |p| into.lock().unwrap().push(p)), redactor, interval, max_bytes);
        (sink, seen)
    }

    #[test]
    fn the_first_line_leaves_at_once_and_a_burst_after_it_waits_for_the_interval() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_millis(250), 8 * 1024);
        sink.line("first");
        assert_eq!(seen.lock().unwrap().len(), 1, "a first line waits for nothing");
        for i in 0..50 {
            sink.line(&format!("burst {i}"));
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "fifty lines inside the interval are one piece that is not there yet");
        sink.tick();
        assert_eq!(seen.lock().unwrap().len(), 1, "a tick inside the interval changes nothing");
        std::thread::sleep(Duration::from_millis(300));
        sink.tick();
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[1].text.lines().count(), 50);
        assert!(got[1].text.starts_with("burst 0") && got[1].text.ends_with("burst 49"));
        assert_eq!((got[0].piece, got[1].piece), (1, 2));
    }

    #[test]
    fn a_line_after_a_quiet_time_leaves_with_what_waited() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_millis(250), 8 * 1024);
        sink.line("a");
        sink.line("b");
        std::thread::sleep(Duration::from_millis(300));
        sink.line("c");
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].text, "b\nc");
    }

    #[test]
    fn output_faster_than_a_piece_holds_loses_its_oldest_lines_and_says_how_many() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_millis(250), 1_000);
        sink.line("start");
        // 200 lines of 20 bytes: a piece holds about 1,000 bytes, so about 50 of them.
        for i in 0..200 {
            sink.line(&format!("line number {i:05}!!"));
        }
        std::thread::sleep(Duration::from_millis(300));
        sink.tick();
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        let second = &got[1];
        assert!(second.skipped >= 140, "{}", second.skipped);
        assert!(second.text.len() <= 1_000, "{}", second.text.len());
        assert!(second.text.ends_with("line number 00199!!"), "the newest line is the one that stays: {}", second.text);
        assert_eq!(second.skipped as usize + second.text.lines().count(), 200, "every line is either shown or counted");
    }

    #[test]
    fn a_secret_is_masked_and_hidden_characters_are_taken_out_before_a_piece_leaves() {
        let mut redactor = Redactor::new();
        redactor.add_secret("hunter2-the-password");
        let (sink, seen) = collect(redactor, Duration::from_millis(10), 8 * 1024);
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        sink.line(&format!("login with hunter2-the-password and {token}"));
        sink.line("colour \u{1b}[31mred\u{1b}[0m and a zero\u{200b}width\u{7} bell");
        sink.finish();
        let all: String = seen.lock().unwrap().iter().map(|p| p.text.clone()).collect::<Vec<_>>().join("\n");
        assert!(!all.contains("hunter2-the-password") && !all.contains(&token), "{all}");
        assert!(all.contains("login with") && all.contains("[REDACTED]"), "{all}");
        assert!(all.contains("colour red and a zerowidth bell"), "{all}");
    }

    #[test]
    fn a_private_key_block_is_dropped_to_its_end_whichever_pieces_its_lines_fall_in() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_millis(1), 8 * 1024);
        sink.line("before");
        std::thread::sleep(Duration::from_millis(5));
        sink.line("-----BEGIN RSA PRIVATE KEY-----");
        std::thread::sleep(Duration::from_millis(5));
        sink.line("MIIEowIBAAKCAQEAxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
        std::thread::sleep(Duration::from_millis(5));
        sink.line("yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy");
        std::thread::sleep(Duration::from_millis(5));
        sink.line("-----END RSA PRIVATE KEY-----");
        std::thread::sleep(Duration::from_millis(5));
        sink.line("after");
        sink.finish();
        let all: String = seen.lock().unwrap().iter().map(|p| p.text.clone()).collect::<Vec<_>>().join("\n");
        assert!(!all.contains("MIIEow") && !all.contains("yyyyyy") && !all.contains("xxxxxx"), "{all}");
        assert!(all.contains("before") && all.contains("after") && all.contains(KEY_DROPPED), "{all}");
        assert_eq!(all.matches(KEY_DROPPED).count(), 1, "one line says it, not one for every line of the key");
    }

    #[test]
    fn a_secret_that_a_cut_would_go_through_does_not_leave_as_a_beginning() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_millis(1), 64 * 1024);
        let token = format!("ghp_{}", "Zq9X".repeat(10));
        // The raw cut is at 16 KiB: put the token so that it begins just before it, and has no space in front of it.
        let line = format!("{}{token}{}", "x".repeat(RAW_LINE_BYTES - 12), "y".repeat(500));
        sink.line(&line);
        sink.finish();
        let all: String = seen.lock().unwrap().iter().map(|p| p.text.clone()).collect::<Vec<_>>().join("\n");
        assert!(!all.contains("ghp_"), "the beginning of a token is left at the end: {}", &all[all.len().saturating_sub(80)..]);
        assert!(all.contains("[line cut]"));
        assert!(all.chars().count() < MAX_LINE_CHARS + 40, "and the line is shown cut");
    }

    #[test]
    fn a_long_line_is_shown_cut_and_a_finished_sink_takes_no_more() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_millis(1), 64 * 1024);
        sink.line(&"w".repeat(5_000));
        sink.finish();
        sink.line("too late");
        sink.tick();
        sink.finish();
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert!(got[0].text.ends_with("… [line cut]") && got[0].text.chars().count() <= MAX_LINE_CHARS + 20);
    }

    #[test]
    fn finish_hands_over_what_waits_once() {
        let (sink, seen) = collect(Redactor::new(), Duration::from_secs(60), 8 * 1024);
        sink.line("one");
        sink.line("two");
        sink.line("three");
        assert_eq!(seen.lock().unwrap().len(), 1);
        sink.finish();
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].text, "two\nthree");
        sink.finish();
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(sink.pieces(), 2);
    }

    #[test]
    fn a_sink_kept_for_a_job_says_so() {
        let (sink, _) = collect(Redactor::new(), Duration::from_millis(1), 8 * 1024);
        assert!(!sink.is_detached());
        let other = sink.clone();
        other.detach();
        assert!(sink.is_detached(), "a clone is the same sink");
    }
}
