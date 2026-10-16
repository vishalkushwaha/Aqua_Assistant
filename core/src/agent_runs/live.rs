//! The live output of the calls of a run (card OA7-04, task TM-14 of the OpenCode audit).
//!
//! A tool whose program prints as it works (`term.run`, `term.start`, `term.shell`) tells the tool host's `ProgressSink` each line; the host cleans the lines like a result, coalesces them to
//! a piece every quarter of a second at most (`aqua_tool_host::progress`) and hands each piece to the emitter that [`source`] made for that call. The emitter puts it on a feed that the
//! streams of `api_gateway::run_api` read: `event: tool.progress` frames, to the UI token only, never rows of the log (a build that prints for ten minutes would fill it), and never in the
//! text the model reads.
//!
//! * **Bounded.** The feed is a broadcast channel of [`CAPACITY`] pieces. A stream that reads slower than the pieces come is told it fell behind and goes on from the newest; a run is never
//!   slowed by it, and a piece is dropped, not queued.
//! * **A call of a recorded run.** The source answers for a call only when the run is being recorded on the thread that makes the call (the agent loop's thread) and a tool call of that run
//!   is open: the step the app shows the lines under is that call's (`call_id`, the number of its step row, the same as in `tool.started` and `tool.finished`). A call made some other way
//!   has no card to show them on, and gets no sink.
//! * **Nothing is kept.** A client that was not connected misses what it was not there for. The model reads only the digest that the call ends with.

use super::events::Event;
use aqua_tool_host::{ProgressCall, ProgressEmit, ProgressSource};
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;

/// How many pieces the feed holds for a stream that has not read them yet.
pub const CAPACITY: usize = 256;

/// One piece of the live output of one call of a run, as the feed carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub run_id: String,
    /// The number of the step row of the call, as `tool.started` says it.
    pub call_id: i64,
    /// The number of the piece inside its call: 1, 2, 3 ...
    pub piece: u64,
    /// The lines, masked, joined with line breaks.
    pub text: String,
    /// How many lines were dropped before this piece because the output came faster than a piece can hold.
    pub skipped: u64,
}

impl Piece {
    /// The frame a stream sends for it: `event: tool.progress`, with the run it is for.
    pub fn frame(&self) -> serde_json::Value {
        super::events::progress_frame(&self.run_id, &Event::ToolProgress { call_id: self.call_id, piece: self.piece, text: self.text.clone(), skipped: self.skipped })
    }
}

fn feed() -> &'static broadcast::Sender<Arc<Piece>> {
    static FEED: OnceLock<broadcast::Sender<Arc<Piece>>> = OnceLock::new();
    FEED.get_or_init(|| broadcast::channel(CAPACITY).0)
}

/// Starts reading the feed. Pieces told before this call are not seen.
pub fn subscribe() -> broadcast::Receiver<Arc<Piece>> {
    feed().subscribe()
}

/// How many streams are reading the feed now.
pub fn listeners() -> usize {
    feed().receiver_count()
}

/// Puts a piece on the feed. Never blocks, and is no error when nobody reads.
pub fn publish(piece: Piece) {
    let _ = feed().send(Arc::new(piece));
}

/// The source the tool host asks for each call of a run (`ToolHost::set_progress_source`). It is asked on the thread that makes the call, which is how it finds the step of the run that is
/// open there. `None` for a call that has no such step: no sink is made and the program's lines go nowhere.
pub fn source() -> ProgressSource {
    Arc::new(|call: &ProgressCall| -> Option<ProgressEmit> {
        let call_id = super::open_call_of(call.run_id)?;
        let run_id = call.run_id.to_string();
        let emit: ProgressEmit = Arc::new(move |p| publish(Piece { run_id: run_id.clone(), call_id, piece: p.piece, text: p.text, skipped: p.skipped }));
        Some(emit)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_tool_host::{ProgressPiece, ProgressSink, Redactor};
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn a_piece_is_a_frame_of_the_run_it_is_for_without_a_number_of_the_log() {
        let piece = Piece { run_id: "run_a".into(), call_id: 7, piece: 2, text: "Compiling x\nCompiling y".into(), skipped: 3 };
        assert_eq!(piece.frame(), json!({"v": 1, "type": "tool.progress", "run_id": "run_a", "call_id": 7, "piece": 2, "text": "Compiling x\nCompiling y", "skipped": 3}));
        assert!(piece.frame().get("seq").is_none() && piece.frame().get("id").is_none(), "a frame is no row: it never moves the number a client takes the stream up again from");
    }

    #[test]
    fn what_is_published_reaches_every_reader_in_order_and_a_reader_that_is_slow_loses_the_oldest_and_never_holds_up_the_writer() {
        let mut fast = subscribe();
        let mut slow = subscribe();
        let before = listeners();
        assert!(before >= 2);
        let started = std::time::Instant::now();
        // (The feed is the process's: the pieces of another test may be on it, so each reader looks only at this test's run.)
        for i in 0..(CAPACITY as u64 + 50) {
            publish(Piece { run_id: "run_slow".into(), call_id: 1, piece: i + 1, text: format!("line {i}"), skipped: 0 });
            // The fast reader keeps up.
            while let Ok(p) = fast.try_recv() {
                if p.run_id == "run_slow" {
                    assert_eq!(p.piece, i + 1);
                }
            }
        }
        assert!(started.elapsed() < Duration::from_secs(2), "publishing never waits for a reader");
        // The slow one was told it fell behind, and then goes on from what is still held, in order.
        assert!(matches!(slow.try_recv(), Err(broadcast::error::TryRecvError::Lagged(n)) if n >= 50), "a reader that is behind is told so");
        let mut mine = Vec::new();
        while let Ok(p) = slow.try_recv() {
            if p.run_id == "run_slow" {
                mine.push(p.piece);
            }
        }
        assert!(mine.len() >= 2 && mine.windows(2).all(|w| w[1] == w[0] + 1), "in order after the gap: {mine:?}");
        assert_eq!(*mine.last().unwrap(), CAPACITY as u64 + 50, "and up to the newest");
    }

    #[test]
    fn publishing_with_no_reader_is_no_error() {
        publish(Piece { run_id: "run_none".into(), call_id: 1, piece: 1, text: "x".into(), skipped: 0 });
    }

    /// The emitter the source makes carries what the sink hands it, under the run and the call it was made for.
    #[test]
    fn the_emitter_of_a_call_puts_its_pieces_on_the_feed_under_its_run_and_call() {
        let mut reader = subscribe();
        let emit: ProgressEmit = {
            let (run_id, call_id) = ("run_emit".to_string(), 12i64);
            Arc::new(move |p: ProgressPiece| publish(Piece { run_id: run_id.clone(), call_id, piece: p.piece, text: p.text, skipped: p.skipped }))
        };
        let sink = ProgressSink::with_limits(emit, Redactor::new(), Duration::from_millis(1), 8 * 1024);
        sink.line("first line");
        sink.finish();
        let mine = loop {
            let p = reader.try_recv().expect("a piece");
            if p.run_id == "run_emit" {
                break p;
            }
        };
        assert_eq!((mine.call_id, mine.piece, mine.text.as_str()), (12, 1, "first line"));
    }
}
