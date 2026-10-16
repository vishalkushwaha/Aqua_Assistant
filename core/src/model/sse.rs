//! The two readers of a model service's answer that are not about what the answer says: the HTTP "chunked" framing, and the server-sent events inside it (card OA9-04, task PR-07).
//!
//! They were written for the local server (`llama_server.rs`, which read the answer of `llama-server` a few characters at a time) and are moved here, unchanged, because the
//! provider that speaks to an Ollama or an LM Studio (`openai_compat.rs`) reads the same bytes in the same way: one copy, one set of tests (those in `llama_server.rs`, which still
//! run them, and the cassette replay, which feeds them the bytes a service really sent in the pieces it sent them in).
//!
//! Neither of them knows what the events mean. [`ChunkedDecoder`] gives the bytes of a chunked body that are complete so far; [`EventSplitter`] gives the `data:` of each complete event.

/// An HTTP "chunked" body, decoded as the bytes arrive (the whole body is not there yet: this is for a reply that is still being
/// written).
#[derive(Default)]
pub struct ChunkedDecoder {
    pending: Vec<u8>,
    state: ChunkState,
}

#[derive(Default, Clone, Copy)]
enum ChunkState {
    /// Waiting for the line that says how long the next chunk is.
    #[default]
    Size,
    /// Inside a chunk, with this many bytes of it still to come.
    Data(usize),
    /// Past a chunk's last byte: the line break after it.
    Break(usize),
    Finished,
}

impl ChunkedDecoder {
    /// The bytes of the body that are complete so far, from the next piece of the stream.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            match self.state {
                ChunkState::Finished => {
                    self.pending.clear();
                    break;
                }
                ChunkState::Size => {
                    let Some(end) = self.pending.windows(2).position(|w| w == b"\r\n") else { break };
                    let line = String::from_utf8_lossy(&self.pending[..end]).to_string();
                    let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
                    self.pending.drain(..end + 2);
                    self.state = if size == 0 { ChunkState::Finished } else { ChunkState::Data(size) };
                }
                ChunkState::Data(left) => {
                    if self.pending.is_empty() {
                        break;
                    }
                    let take = left.min(self.pending.len());
                    out.extend(self.pending.drain(..take));
                    self.state = if take == left { ChunkState::Break(2) } else { ChunkState::Data(left - take) };
                }
                ChunkState::Break(left) => {
                    if self.pending.is_empty() {
                        break;
                    }
                    let take = left.min(self.pending.len());
                    self.pending.drain(..take);
                    self.state = if take == left { ChunkState::Size } else { ChunkState::Break(left - take) };
                }
            }
        }
        out
    }

    /// Has the last chunk (the one of size 0) been read? A body that is cut off in the middle of its framing is not finished.
    pub fn finished(&self) -> bool {
        matches!(self.state, ChunkState::Finished)
    }
}

/// Server-sent events, cut out of a stream of text as it arrives: the `data:` of each complete event (events end at a blank line).
#[derive(Default)]
pub struct EventSplitter {
    /// Bytes, not text: a read can end in the middle of a character (a degree sign, an accent, an emoji), and the end of an event is a
    /// blank line, which is plain ASCII. An event is turned into text only when it is whole, or each half of such a character would
    /// be spoken as a replacement mark.
    pending: Vec<u8>,
}

/// Where the first event of `bytes` ends, as the number of bytes it takes up to and with its blank line. The servers of the local models end a line with `\n` (llama-server, Ollama,
/// LM Studio, vLLM); the standard also allows `\r\n`, which a proxy or another server may write, so a blank line of either kind ends an event.
fn event_end(bytes: &[u8]) -> Option<usize> {
    let lf = bytes.windows(2).position(|w| w == b"\n\n").map(|at| at + 2);
    let crlf = bytes.windows(4).position(|w| w == b"\r\n\r\n").map(|at| at + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl EventSplitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = event_end(&self.pending) {
            let raw: Vec<u8> = self.pending.drain(..end).collect();
            let event = String::from_utf8_lossy(&raw);
            let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect();
            if !data.is_empty() {
                events.push(data.join("\n"));
            }
        }
        events
    }

    /// How many bytes are held back waiting for the end of an event. A service that never ends one must not make this grow without a bound: the caller reads it.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_ends_at_a_blank_line_of_either_kind_and_a_character_cut_between_reads_comes_out_whole() {
        let mut splitter = EventSplitter::default();
        assert!(splitter.feed(b"data: one\n").is_empty(), "an event is not whole until its blank line");
        assert_eq!(splitter.feed(b"\ndata: two\r\n\r\nevent: x\n"), ["one", "two"], "a blank line is two line feeds or two carriage-return line feeds");
        assert_eq!(splitter.feed(b"data: three\n\n"), ["three"], "a line that is not data is kept with its event and is not data");
        let text = "20\u{b0} outside";
        let bytes = format!("data: {text}\n\n").into_bytes();
        let at = bytes.iter().position(|b| *b == 0xC2).unwrap() + 1;
        let (a, b) = bytes.split_at(at);
        assert!(splitter.feed(a).is_empty());
        assert_eq!(splitter.feed(b), [text], "the two halves of the degree sign are one character");
        assert_eq!(splitter.feed(b"data: a\ndata: b\n\n"), ["a\nb"], "an event of two data lines is joined by a line break");
        assert_eq!(splitter.feed(b": a comment\n\n"), Vec::<String>::new(), "a comment is no event");
        assert_eq!(splitter.pending_len(), 0);
        splitter.feed(b"data: no end");
        assert_eq!(splitter.pending_len(), 12, "what waits for its end is counted, so a caller can bound it");
    }

    #[test]
    fn the_chunked_body_is_given_as_it_is_complete_and_says_when_it_is_finished() {
        let body = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        let mut whole = ChunkedDecoder::default();
        assert_eq!(whole.feed(body), b"hello world");
        assert!(whole.finished());
        // One byte at a time gives the same, and it is finished from the last chunk's size line on.
        let mut slow = ChunkedDecoder::default();
        let mut got = Vec::new();
        for byte in body.iter() {
            got.extend(slow.feed(&[*byte]));
        }
        assert_eq!(got, b"hello world");
        assert!(slow.finished());
        // A body that is cut is not finished.
        let mut cut = ChunkedDecoder::default();
        assert_eq!(cut.feed(b"a\r\nabcde"), b"abcde");
        assert!(!cut.finished());
    }
}
