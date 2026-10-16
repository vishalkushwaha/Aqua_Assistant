//! The framing of a language server's messages (card OA12-04): `Content-Length: N`, an empty line, then N bytes of JSON.
//!
//! What a server writes is read with every size bounded: a header line is at most 1,024 bytes, there are at most 16 header lines, and a body larger than the cap the caller gives is read in
//! small pieces and thrown away (the stream stays in step, and the caller says that the answer was too large). A header that is not well formed, a length that is not a number or is
//! absurd, a body that ends early: each is an error, and the caller ends the server.

use std::io::BufRead;
#[cfg(test)]
use std::io::Read;

/// The longest header line, and the most header lines, of one message.
const MAX_HEADER_LINE: usize = 1024;
const MAX_HEADERS: usize = 16;
/// A body longer than this is not even skipped: the length is taken for a mistake or an attack.
pub const MAX_SKIPPED_BODY: u64 = 512 << 20;

/// What was read.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// The body of a message, at most the cap.
    Body(Vec<u8>),
    /// A message larger than the cap, read and thrown away: its size.
    Skipped(u64),
}

/// Why no message was read.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The stream ended between two messages.
    Eof,
    /// The stream ended inside a message.
    Truncated,
    /// What the server wrote is not the framing: the sentence says what is wrong with it.
    Malformed(&'static str),
    Io(String),
}

/// A message ready to be written: its header and its body.
pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read one message from `r`. A body longer than `max_body` is skipped (see [`Frame::Skipped`]).
pub fn read_frame<R: BufRead>(r: &mut R, max_body: usize) -> Result<Frame, FrameError> {
    let mut length: Option<u64> = None;
    let mut lines = 0usize;
    loop {
        let line = match read_line(r, lines == 0)? {
            Some(l) => l,
            None => return Err(FrameError::Eof),
        };
        if line.is_empty() {
            break;
        }
        lines += 1;
        if lines > MAX_HEADERS {
            return Err(FrameError::Malformed("too many header lines"));
        }
        let Some((name, value)) = line.split_once(':') else { return Err(FrameError::Malformed("a header line without a colon")) };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let v = value.trim();
            if length.is_some() {
                return Err(FrameError::Malformed("two Content-Length headers"));
            }
            if v.is_empty() || v.len() > 18 || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(FrameError::Malformed("a Content-Length that is not a number"));
            }
            length = v.parse::<u64>().ok();
        }
    }
    let Some(n) = length else { return Err(FrameError::Malformed("no Content-Length header")) };
    if n > MAX_SKIPPED_BODY {
        return Err(FrameError::Malformed("a Content-Length that is absurd"));
    }
    if n > max_body as u64 {
        let mut left = n;
        let mut sink = [0u8; 8192];
        while left > 0 {
            let want = left.min(sink.len() as u64) as usize;
            match r.read(&mut sink[..want]) {
                Ok(0) => return Err(FrameError::Truncated),
                Ok(k) => left -= k as u64,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(FrameError::Io(e.to_string())),
            }
        }
        return Ok(Frame::Skipped(n));
    }
    let mut body = vec![0u8; n as usize];
    let mut got = 0usize;
    while got < body.len() {
        match r.read(&mut body[got..]) {
            Ok(0) => return Err(FrameError::Truncated),
            Ok(k) => got += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e.to_string())),
        }
    }
    Ok(Frame::Body(body))
}

/// One header line without its `\r\n`. `None` when the stream ended before the first byte of the first line (`first`); a stream that ends later is `Truncated`.
fn read_line<R: BufRead>(r: &mut R, first: bool) -> Result<Option<String>, FrameError> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let (done, used) = {
            let buf = match r.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(FrameError::Io(e.to_string())),
            };
            if buf.is_empty() {
                return if first && line.is_empty() { Ok(None) } else { Err(FrameError::Truncated) };
            }
            match buf.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    line.extend_from_slice(&buf[..i]);
                    (true, i + 1)
                }
                None => {
                    line.extend_from_slice(buf);
                    (false, buf.len())
                }
            }
        };
        r.consume(used);
        if line.len() > MAX_HEADER_LINE {
            return Err(FrameError::Malformed("a header line that is too long"));
        }
        if done {
            break;
        }
    }
    if line.last() != Some(&b'\r') {
        return Err(FrameError::Malformed("a header line that does not end with CR LF"));
    }
    line.pop();
    // A header is text; a byte that is not ASCII is not the framing.
    if !line.is_ascii() {
        return Err(FrameError::Malformed("a header line that is not ASCII"));
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// A reader that gives its bytes a few at a time, to test that a message split anywhere is read whole.
#[cfg(test)]
pub struct Trickle<'a>(pub &'a [u8], pub usize);

#[cfg(test)]
impl Read for Trickle<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.1.min(out.len()).min(self.0.len());
        out[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn read(bytes: &[u8], max: usize) -> Result<Frame, FrameError> {
        read_frame(&mut Cursor::new(bytes.to_vec()), max)
    }

    #[test]
    fn a_message_is_read_by_its_length_and_two_in_a_row_are_two() {
        let mut bytes = encode(br#"{"a":1}"#);
        bytes.extend(encode(br#"{"b":22}"#));
        let mut c = Cursor::new(bytes);
        assert_eq!(read_frame(&mut c, 100), Ok(Frame::Body(br#"{"a":1}"#.to_vec())));
        assert_eq!(read_frame(&mut c, 100), Ok(Frame::Body(br#"{"b":22}"#.to_vec())));
        assert_eq!(read_frame(&mut c, 100), Err(FrameError::Eof));
    }

    #[test]
    fn a_message_split_into_one_byte_pieces_is_read_whole() {
        let bytes = encode(br#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        let mut r = BufReader::with_capacity(4, Trickle(&bytes, 1));
        assert_eq!(read_frame(&mut r, 1000), Ok(Frame::Body(br#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_vec())));
    }

    #[test]
    fn other_headers_and_the_case_of_the_name_do_not_matter() {
        let bytes = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}".to_vec();
        assert_eq!(read(&bytes, 10), Ok(Frame::Body(b"{}".to_vec())));
    }

    #[test]
    fn a_message_over_the_cap_is_skipped_and_the_next_one_is_still_read() {
        let mut bytes = encode(&vec![b'x'; 50_000]);
        bytes.extend(encode(b"{}"));
        let mut c = Cursor::new(bytes);
        assert_eq!(read_frame(&mut c, 1000), Ok(Frame::Skipped(50_000)));
        assert_eq!(read_frame(&mut c, 1000), Ok(Frame::Body(b"{}".to_vec())));
    }

    #[test]
    fn the_framing_that_is_not_the_framing_is_an_error() {
        let cases: [(&[u8], &str); 9] = [
            (b"garbage without a colon\r\n\r\n", "a header line without a colon"),
            (b"Content-Length: abc\r\n\r\n{}", "not a number"),
            (b"Content-Length: -5\r\n\r\n{}", "not a number"),
            (b"Content-Length: 99999999999999999999\r\n\r\n{}", "not a number"),
            (b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}", "two Content-Length"),
            (b"Content-Type: x\r\n\r\n{}", "no Content-Length"),
            (b"Content-Length: 2\n\n{}", "CR LF"),
            (b"Content-Length: 600000000\r\n\r\n", "absurd"),
            (b"Content-Length: 2\r\n\xff\xfe: x\r\n\r\n{}", "ASCII"),
        ];
        for (bytes, why) in cases {
            match read(bytes, 100) {
                Err(FrameError::Malformed(w)) => assert!(w.contains(why), "{w} for {why}"),
                other => panic!("{why}: {other:?}"),
            }
        }
        let long = format!("X: {}\r\n\r\n", "a".repeat(5000));
        assert!(matches!(read(long.as_bytes(), 100), Err(FrameError::Malformed(w)) if w.contains("too long")));
        let many = format!("{}\r\n", "A: b\r\n".repeat(40));
        assert!(matches!(read(many.as_bytes(), 100), Err(FrameError::Malformed(w)) if w.contains("too many")));
    }

    #[test]
    fn a_stream_that_ends_inside_a_message_is_truncated_and_one_that_ends_between_messages_is_eof() {
        assert_eq!(read(b"", 100), Err(FrameError::Eof));
        assert_eq!(read(b"Content-Length: 10\r\n\r\n{}", 100), Err(FrameError::Truncated));
        assert_eq!(read(b"Content-Length: 10", 100), Err(FrameError::Truncated));
        assert_eq!(read(b"Content-Length: 5000\r\n\r\nabc", 10), Err(FrameError::Truncated), "a skipped body that ends early too");
    }
}
