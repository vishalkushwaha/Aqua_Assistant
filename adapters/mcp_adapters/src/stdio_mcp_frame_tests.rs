//! Property-style test of the reader of a server's messages (card OA13-01, task QA-06): whatever the bytes are and however a pipe cuts them into pieces, `read_bounded_line` gives the
//! same messages, reports each message that is over the limit once, and is back in step at the next one. It is a module of the crate and not a file of `tests/` because the function is
//! private; the case loop is the one the other property tests use (`tests/prop_kit/mod.rs` of `aqua-tool-host`).

#[path = "../../tool_host/tests/prop_kit/mod.rs"]
mod prop_kit;

use super::read_bounded_line;
use prop_kit::{check, chunks, Rng};
use std::io::{BufRead, Read};

/// What a pipe does: hands over a few bytes at a time (the sizes are the test's dice), and ends.
struct Pipe<'a> {
    pieces: Vec<&'a [u8]>,
    at: usize,
    inside: usize,
}

impl<'a> Pipe<'a> {
    fn new(pieces: Vec<&'a [u8]>) -> Self {
        Pipe { pieces, at: 0, inside: 0 }
    }
}

impl Read for Pipe<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let buf = self.fill_buf()?;
        let n = buf.len().min(out.len());
        out[..n].copy_from_slice(&buf[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Pipe<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.at < self.pieces.len() && self.inside >= self.pieces[self.at].len() {
            self.at += 1;
            self.inside = 0;
        }
        Ok(self.pieces.get(self.at).map(|p| &p[self.inside..]).unwrap_or(&[]))
    }

    fn consume(&mut self, n: usize) {
        self.inside += n;
    }
}

/// What the reader must give, said the plain way: the bytes cut after each `\n`, the last piece with no `\n` kept when it is not empty, and a piece longer than `max` (its `\n`
/// counted) an error.
fn expected(data: &[u8], max: usize) -> Vec<Result<String, ()>> {
    let mut out = Vec::new();
    let mut from = 0;
    while from < data.len() {
        let end = data[from..].iter().position(|b| *b == b'\n').map(|i| from + i + 1).unwrap_or(data.len());
        let piece = &data[from..end];
        out.push(if piece.len() > max { Err(()) } else { Ok(String::from_utf8_lossy(piece).into_owned()) });
        from = end;
    }
    out
}

fn read_all<R: BufRead>(reader: &mut R, max: usize) -> Vec<Result<String, ()>> {
    let mut out = Vec::new();
    loop {
        match read_bounded_line(reader, max) {
            Ok(Some(line)) => out.push(Ok(line)),
            Ok(None) => return out,
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "an over-long message is reported as invalid data");
                out.push(Err(()));
            }
        }
        assert!(out.len() < 10_000, "the reader does not end");
    }
}

/// A stream of lines: some short, some about as long as the limit, some over it, with a last line that may have no end, and the odd byte that is not UTF-8.
fn stream(rng: &mut Rng, max: usize) -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..rng.range(0, 8) {
        let len = match rng.below(4) {
            0 => rng.range(0, 3),
            1 => rng.range(0, max),
            2 => max.saturating_sub(rng.range(0, 2)) + rng.range(0, 2),
            _ => rng.range(max, max * 4),
        };
        let line = if rng.chance(15) { rng.bytes(len) } else { (0..len).map(|_| b'a' + rng.below(26) as u8).collect() };
        data.extend(line.into_iter().filter(|b| *b != b'\n'));
        data.push(b'\n');
    }
    if rng.chance(40) {
        let tail = rng.range(0, max + 3);
        data.extend((0..tail).map(|_| b'z'));
    }
    data
}

#[test]
fn the_reader_gives_the_same_messages_whatever_size_the_pipe_cuts_the_bytes_into() {
    check("frames_chunked", 800, |rng, _| {
        let max = rng.range(1, 40);
        let data = stream(rng, max);
        let want = expected(&data, max);
        // As one block.
        assert_eq!(read_all(&mut std::io::Cursor::new(data.clone()), max), want, "max {max}, data {:?}", String::from_utf8_lossy(&data));
        // In pieces of any size, down to a byte at a time.
        let piece = rng.range(1, 12);
        let mut pipe = Pipe::new(chunks(rng, &data, piece));
        assert_eq!(read_all(&mut pipe, max), want, "max {max}, pieces up to {piece}, data {:?}", String::from_utf8_lossy(&data));
    });
}

#[test]
fn a_message_over_the_limit_is_reported_once_and_the_next_one_is_read_whole() {
    check("frames_over_limit", 400, |rng, _| {
        let max = rng.range(4, 30);
        let mut data = vec![b'x'; rng.range(max, max * 5)];
        data.push(b'\n');
        data.extend_from_slice(b"{\"ok\":true}\n");
        let piece = rng.range(1, 9);
        let mut pipe = Pipe::new(chunks(rng, &data, piece));
        if max < 12 {
            // The good message is over the limit too: two errors, then the end.
            assert_eq!(read_all(&mut pipe, max), vec![Err(()), Err(())]);
        } else {
            assert_eq!(read_all(&mut pipe, max), vec![Err(()), Ok("{\"ok\":true}\n".to_string())], "max {max}");
        }
    });
}

#[test]
fn no_stream_makes_the_reader_panic_or_return_a_line_over_the_limit() {
    check("frames_random_bytes", 600, |rng, _| {
        let n = rng.range(0, 300);
        let data = rng.bytes(n);
        let max = rng.range(1, 64);
        let piece = rng.range(1, 20);
        let mut pipe = Pipe::new(chunks(rng, &data, piece));
        let mut total = 0usize;
        for item in read_all(&mut pipe, max) {
            if let Ok(line) = item {
                // Lossy text can be longer than the bytes it came from (each bad byte becomes three), so the bytes are what is held to the limit: a line of `max` bytes is at most `3 * max` here.
                assert!(line.len() <= max * 3, "a message of {} bytes came back for a limit of {max}", line.len());
                total += line.len();
            }
        }
        let _ = total;
    });
}
