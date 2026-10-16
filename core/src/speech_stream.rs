//! Cutting an answer into pieces that can be spoken one at a time, as it is being written.
//!
//! The model writes about 6 tokens a second on this laptop, so a three-sentence answer takes 10 seconds, and speaking only when it is
//! complete means 10 seconds of silence. The first piece is spoken while the rest is still being written. A short first sentence is
//! kept whole. A long first sentence is released after about 10 to 15 words, at a comma when there is one, otherwise at a word
//! boundary, so speech can start before that sentence ends. Later pieces wait for a sentence of 60 characters or more (fewer, longer
//! pieces sound better and cost fewer calls to the speech engine). A full stop is not the end
//! of a sentence inside "3.5", "Dr.", "e.g.", "p.m." or an initial ("J. Smith"), and nothing is a sentence end until the character
//! after it has arrived. What the model thinks (`<think>...</think>`) is never spoken, even when a tag arrives in two pieces.
//!
//! Code is for the screen, not for the ear (card OA10-06, UX-08): a splitter made with [`SentenceSplitter::code_aware`] leaves out a fenced block (three or more backticks or tildes, a
//! longer fence needs a closing fence as long) and a unified diff written outside a fence, and says [`CODE_CUE`] once in its place. The filter ([`CodeFilter`]) works on whole lines and
//! lets prose through as it arrives: only a line that may still turn out to be a fence or a diff header waits, and only until it is known not to be one. [`SentenceSplitter::new`] is
//! what it always was: whether a turn is code-aware is decided by the caller (`AQUA_CODE_SPEECH`, see `coding_voice::code_speech_on`).

/// The shortest first piece, in spoken characters: about 6 words, 1.5 seconds of speech. A finished sentence shorter than this
/// is kept and joined to what follows, unless it is all the answer has.
pub const FIRST_PIECE_MIN: usize = 25;
/// The shortest later piece. Each one is a call to the speech engine, and a few long pieces sound better than many short ones.
pub const LATER_PIECE_MIN: usize = 60;
/// A long first sentence is not held until it ends. Once this many words are complete, a comma (or similar) may end the first piece.
const FIRST_EARLY_MIN_WORDS: usize = 10;
/// The first piece never waits for more than this many words, even when the sentence has not ended.
const FIRST_EARLY_MAX_WORDS: usize = 15;
/// A run of words with no sentence end at all is split anyway, at a comma or a space, once it is this long.
const LONGEST_RUN: usize = 200;

/// Words that end in a full stop without ending a sentence (lower case, without the stop).
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "mt", "vs", "no", "inc", "ltd", "co", "corp", "e.g", "i.e", "a.m", "p.m", "u.s", "u.k", "approx", "dept", "est", "fig", "gen", "gov", "hon", "rev", "sgt",
    "col", "capt", "lt", "cf", "al",
];

/// What a model may write before its answer and that is not part of it: the opening tag and the tag that ends it.
const REASONING: [(&str, &str); 4] = [("<think>", "</think>"), ("<thinking>", "</thinking>"), ("<reason>", "</reason>"), ("<reasoning>", "</reasoning>")];

/// Cuts a stream of text into spoken pieces. Feed it each bit of text as it arrives with [`SentenceSplitter::push`], and call
/// [`SentenceSplitter::finish`] at the end for what is left.
#[derive(Default)]
pub struct SentenceSplitter {
    /// Arrived and not yet sorted into spoken text and thinking: its end may be the start of a tag, so it waits for more.
    raw: String,
    /// Spoken text that has not been handed out yet.
    pending: String,
    /// The tag that ends the thinking the text is in the middle of, if it is.
    inside: Option<&'static str>,
    handed_out: usize,
    /// Leaves code out of what is spoken (see [`CodeFilter`]); `None` for a splitter that speaks everything, as it always did.
    code: Option<CodeFilter>,
}

impl SentenceSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// A splitter that does not speak code: a fenced block and a diff are left out, and [`CODE_CUE`] is said once in the place of the first.
    pub fn code_aware() -> Self {
        Self { code: Some(CodeFilter::default()), ..Self::default() }
    }

    /// Text that is spoken text (not thinking) goes to what is waiting to be handed out, through the code filter when there is one.
    fn admit(&mut self, text: &str) {
        match &mut self.code {
            Some(filter) => {
                let kept = filter.push(text);
                self.pending.push_str(&kept);
            }
            None => self.pending.push_str(text),
        }
    }

    /// The pieces that are complete now that `text` has arrived.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.raw.push_str(text);
        self.sort();
        let mut out = Vec::new();
        while let Some(end) = self.next_piece_end() {
            let piece: String = self.pending.drain(..end).collect();
            let spoken = speakable(&piece);
            if !spoken.is_empty() {
                self.handed_out += 1;
                out.push(spoken);
            }
        }
        out
    }

    /// What is left at the end of the answer. An answer that was cut off at the token limit (`cut_off`) ends at its last complete
    /// sentence, as the written answer does (see `end_at_last_sentence` in the engine): the unfinished one is dropped, unless nothing
    /// has been said yet and there is no complete sentence, when it is all there is.
    pub fn finish(&mut self, cut_off: bool) -> Option<String> {
        if self.inside.is_none() {
            // What was held back in case it was a tag was not one.
            let held = std::mem::take(&mut self.raw);
            self.admit(&held);
        }
        self.raw.clear();
        // The line the code filter was still waiting to know about (it is prose, or code that was never closed: the filter says which).
        if let Some(filter) = &mut self.code {
            let rest = filter.finish();
            self.pending.push_str(&rest);
        }
        let mut rest = std::mem::take(&mut self.pending);
        if cut_off {
            match sentence_ends(&rest, true).last() {
                Some(&end) => rest.truncate(end),
                None if self.handed_out > 0 => rest.clear(),
                None => {}
            }
        }
        let spoken = speakable(&rest);
        (!spoken.is_empty()).then_some(spoken)
    }

    /// Move what has arrived from `raw` to `pending`, leaving out the thinking. The last few characters stay in `raw` while they
    /// could be the start of a tag.
    fn sort(&mut self) {
        loop {
            match self.inside {
                Some(close) => match self.raw.find(close) {
                    Some(at) => {
                        self.raw.drain(..at + close.len());
                        self.inside = None;
                    }
                    None => {
                        let keep = partial_tag_len(&self.raw, &[close]);
                        self.raw.drain(..self.raw.len() - keep);
                        return;
                    }
                },
                None => {
                    let opened = REASONING.iter().filter_map(|&(open, close)| self.raw.find(open).map(|at| (at, open, close))).min_by_key(|&(at, _, _)| at);
                    match opened {
                        Some((at, open, close)) => {
                            let before: String = self.raw.drain(..at + open.len()).collect();
                            self.admit(&before[..at]);
                            self.inside = Some(close);
                        }
                        None => {
                            let opens: Vec<&str> = REASONING.iter().map(|&(open, _)| open).collect();
                            let keep = partial_tag_len(&self.raw, &opens);
                            let released: String = self.raw.drain(..self.raw.len() - keep).collect();
                            self.admit(&released);
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Where the next piece ends (a byte index into `pending`), if one is complete.
    fn next_piece_end(&self) -> Option<usize> {
        let text = &self.pending;
        if self.handed_out == 0 {
            if let Some(end) = first_piece_end(text) {
                return Some(end);
            }
        }
        let minimum = if self.handed_out == 0 { FIRST_PIECE_MIN } else { LATER_PIECE_MIN };
        let sentence = sentence_ends(text, false).into_iter().find(|&end| speakable(&text[..end]).chars().count() >= minimum);
        let run_limit = text.char_indices().nth(LONGEST_RUN).map(|(i, _)| i);
        match (sentence, run_limit) {
            // The sentence ends too late: cut the run of words before it instead.
            (Some(end), Some(limit)) if end > limit => cut_at_word(text, limit),
            (Some(end), _) => Some(end),
            (None, Some(limit)) => cut_at_word(text, limit),
            (None, None) => None,
        }
    }
}

/// The first piece only. A finished sentence that is already long enough and no more than [`FIRST_EARLY_MAX_WORDS`] words is
/// spoken whole. A longer sentence is released once 15 words are complete, at a comma in the 10-to-15 word window when there
/// is one, otherwise after word 15, so the speech engine starts before the sentence ends. After this piece,
/// [`SentenceSplitter::next_piece_end`] uses the later-piece rules.
fn first_piece_end(text: &str) -> Option<usize> {
    if let Some(end) = sentence_ends(text, false).into_iter().find(|&end| speakable(&text[..end]).chars().count() >= FIRST_PIECE_MIN) {
        if text[..end].split_whitespace().count() <= FIRST_EARLY_MAX_WORDS {
            return Some(end);
        }
    }
    early_word_end(text)
}

/// A cut after a complete word (one that already has white space after it), inside the 10-to-15 word window.
fn early_word_end(text: &str) -> Option<usize> {
    let words = complete_words(text);
    if words.len() < FIRST_EARLY_MIN_WORDS {
        return None;
    }
    // Wait until the upper bound. Cutting at the first comma inside the window would split a sentence that is about to
    // end within 15 words, and the speech engine would be called twice for one short answer.
    if words.len() < FIRST_EARLY_MAX_WORDS {
        return None;
    }
    let window = &words[FIRST_EARLY_MIN_WORDS - 1..FIRST_EARLY_MAX_WORDS];
    let cut = window.iter().rev().find(|word| word.punct).unwrap_or(&words[FIRST_EARLY_MAX_WORDS - 1]);
    Some(cut.cut)
}

struct CompleteWord {
    /// Byte index where this piece ends: after the word and the spaces that follow it, but before a line break.
    cut: usize,
    /// The word itself ends in a comma, semicolon, colon, dash, or ellipsis.
    punct: bool,
}

/// Words that will not grow if more text arrives, because white space has already followed them.
fn complete_words(text: &str) -> Vec<CompleteWord> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1.is_whitespace() {
            i += 1;
            continue;
        }
        while i < chars.len() && !chars[i].1.is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let punct = matches!(chars[i - 1].1, ',' | ';' | ':' | '\u{2014}' | '\u{2013}' | '\u{2026}');
        let cut = if chars[i].1 == '\n' {
            chars[i].0
        } else {
            while i < chars.len() && chars[i].1.is_whitespace() && chars[i].1 != '\n' {
                i += 1;
            }
            chars.get(i).map(|&(b, _)| b).unwrap_or(text.len())
        };
        out.push(CompleteWord { cut, punct });
    }
    out
}

/// The last comma or, failing that, the last space before `limit`, as the end of a piece (just after it).
fn cut_at_word(text: &str, limit: usize) -> Option<usize> {
    let head = &text[..limit];
    head.rfind(", ").map(|i| i + 2).or_else(|| head.rfind(' ').map(|i| i + 1)).filter(|&i| i > 0)
}

/// All the pieces of a whole text (for an answer that was not streamed: the agent's, a local one).
pub fn pieces_of(text: &str) -> Vec<String> {
    let mut splitter = SentenceSplitter::new();
    let mut pieces = splitter.push(text);
    pieces.extend(splitter.finish(false));
    pieces
}

/// Like [`pieces_of`], for a splitter that does not speak code (see [`SentenceSplitter::code_aware`]).
pub fn pieces_of_code_aware(text: &str) -> Vec<String> {
    let mut splitter = SentenceSplitter::code_aware();
    let mut pieces = splitter.push(text);
    pieces.extend(splitter.finish(false));
    pieces
}

/// What is said in the place of code (card OA10-06, UX-08): once for an answer, however many blocks it has.
pub const CODE_CUE: &str = "The code is on the screen.";

/// A fence may be indented this far (a block in a nested list is).
const MAX_FENCE_INDENT: usize = 12;
/// A line that starts like a fence and is longer than this is prose: an info string is a word or two.
const MAX_FENCE_LINE: usize = 120;
/// A line that starts like the header of a diff and is longer than this is prose.
const MAX_MARKER_LINE: usize = 400;
/// The words a diff header line begins with (at the start of the line, as git writes them).
const MARKER_STARTS: [&str; 5] = ["--- ", "+++ ", "@@ ", "diff --git ", "index "];

/// What is done with the rest of the line that is being written.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum LineMode {
    /// The line so far may still be a fence or a diff header: it waits.
    #[default]
    Undecided,
    /// The line is prose: what has been written is out, and so is what comes.
    Pass,
    /// The line is code: what has been written is gone, and so is what comes.
    Drop,
}

/// Leaves code out of text that is written piece by piece, and says [`CODE_CUE`] once in the place of the first block (card OA10-06, UX-08).
///
/// **What is code:** the lines of a fenced block (an opening line of three or more backticks or tildes, up to [`MAX_FENCE_INDENT`] spaces in, and the lines to the closing line of the
/// same character and at least as many; a block that is never closed is code to the end), and a unified diff written outside a fence: two header lines in a row (`--- a/x`, `+++ b/x`,
/// `@@ -1,3 +1,4 @@`, `diff --git ...`, `index abc..def`) and the lines that follow them that start with `+`, `-`, a space, `\` or `@`, up to a blank line or a line that is none of those. One
/// header line alone is not a diff (`---` is a rule, `--- see below` is prose), so it is held until the next line is known and then let through.
///
/// **How it waits:** by lines, but prose is not held back. A line is `Pass` as soon as its first characters cannot be the start of a fence or a header ("The" cannot, "`" and "-" might),
/// and what has been written of it goes out then. Inline code (`like this`) is prose here and the speech cleaner takes its backticks away. The same text gives the same output however it
/// is cut into pieces.
#[derive(Debug, Default)]
pub struct CodeFilter {
    /// The line being written, while it is [`LineMode::Undecided`].
    line: String,
    mode: LineMode,
    /// Inside a fenced block: the character of the fence and how many of it opened the block.
    fence: Option<(char, usize)>,
    /// Inside a diff that is not fenced.
    in_diff: bool,
    /// A complete header line (with its line break) that waits to see whether the next line is a header too.
    held_header: Option<String>,
    cue_said: bool,
}

/// The fence an opening line makes: the character and how many of it, or `None` when the line is not one (a backtick fence's info string has no backtick in it, so "```x```" is inline code).
fn fence_opening(line: &str) -> Option<(char, usize)> {
    let t = line.trim_end_matches('\r').trim_start_matches(' ');
    if line.len() - line.trim_start_matches(' ').len() > MAX_FENCE_INDENT {
        return None;
    }
    let fc = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = t.chars().take_while(|&c| c == fc).count();
    if run < 3 || (fc == '`' && t[run..].contains('`')) {
        return None;
    }
    Some((fc, run))
}

/// Does this complete line close a block opened with `(fc, n)`: only that character, at least `n` of it, and nothing else but white space?
fn closes_fence(line: &str, (fc, n): (char, usize)) -> bool {
    let t = line.trim_end_matches('\r').trim();
    t.chars().count() >= n && t.chars().all(|c| c == fc)
}

/// Is this complete line the header of a diff? Written at the start of the line, as git does.
fn is_diff_header(line: &str) -> bool {
    let t = line.trim_end_matches('\r').trim_end();
    if let Some(rest) = t.strip_prefix("index ") {
        let hex = |s: &str| s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit());
        return rest.split_whitespace().next().and_then(|ids| ids.split_once("..")).is_some_and(|(a, b)| hex(a) && hex(b));
    }
    t.starts_with("diff --git ") || ((t.starts_with("--- ") || t.starts_with("+++ ")) && t.len() > 4) || (t.starts_with("@@ -") && t[3..].contains(" @@"))
}

/// Can the line so far (not yet ended) still turn out to be a diff header? A line that starts like one is held to its end, so this says "no" as early as it can, and the line is not held for
/// long when it is prose ("index 3 is the one", "diffusion ...").
fn may_be_header(line: &str) -> bool {
    if line.len() > MAX_MARKER_LINE {
        return false;
    }
    if let Some(rest) = line.strip_prefix("index ") {
        return rest.chars().take_while(|&c| c != '.').all(|c| c.is_ascii_hexdigit()) && rest.len() <= 80;
    }
    if let Some(rest) = line.strip_prefix("@@ ") {
        return rest.is_empty() || rest.starts_with('-');
    }
    MARKER_STARTS.iter().any(|m| line.starts_with(m) || m.starts_with(line.trim_end_matches('\r')))
}

/// Is this complete line one that a diff goes on with: a changed line, a context line (which starts with a space), a "no newline" note, or a hunk header?
fn is_diff_body(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    matches!(line.chars().next(), Some('+' | '-' | ' ' | '\\' | '@')) || is_diff_header(line)
}

impl CodeFilter {
    /// Takes more of the text; gives the part of it that may be spoken now.
    pub fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if c == '\n' {
                self.end_line(&mut out);
                continue;
            }
            match self.mode {
                LineMode::Pass => out.push(c),
                LineMode::Drop => {}
                LineMode::Undecided => {
                    self.line.push(c);
                    self.decide(&mut out);
                }
            }
        }
        out
    }

    /// The end of the text: the line that was waiting is dealt with as if it had ended, a header that was waiting is let through, and an unclosed block is dropped without a word.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.mode == LineMode::Undecided && !self.line.is_empty() {
            self.end_line(&mut out);
            // `end_line` ends a line that is let through with a line break; the text did not have one.
            if out.ends_with('\n') && !out.ends_with("\n\n") {
                out.pop();
            }
        }
        self.mode = LineMode::Undecided;
        self.line.clear();
        self.fence = None;
        self.in_diff = false;
        if let Some(held) = self.held_header.take() {
            out.push_str(held.trim_end_matches('\n'));
        }
        out
    }

    fn say_cue(&mut self, out: &mut String) {
        if !self.cue_said {
            self.cue_said = true;
            out.push_str(CODE_CUE);
            out.push('\n');
        }
    }

    /// A header that waited is prose after all: it goes out, in its place before the line that was found not to be one.
    fn release_held(&mut self, out: &mut String) {
        if let Some(held) = self.held_header.take() {
            out.push_str(&held);
        }
    }

    /// Looks at the line so far and settles what is certain: that it is prose (and out it goes), or code (and it is gone), or that more must be seen.
    fn decide(&mut self, out: &mut String) {
        let indent = self.line.len() - self.line.trim_start_matches(' ').len();
        let t = self.line.trim_start_matches(' ');
        if let Some((fc, _)) = self.fence {
            // Inside a block only a closing line matters, and it has nothing but the fence character on it.
            if !t.is_empty() && !t.trim_end().chars().all(|c| c == fc) {
                self.line.clear();
                self.mode = LineMode::Drop;
            }
            return;
        }
        if self.in_diff {
            // The first character says whether the line goes on with the diff (a context line starts with a space, so the line is looked at whole).
            let first = self.line.chars().next();
            match first {
                Some('+' | '-' | ' ' | '\\' | '@') => {
                    self.line.clear();
                    self.mode = LineMode::Drop;
                    return;
                }
                Some('d' | 'i') if may_be_header(&self.line) => return,
                _ => {
                    // Not a diff line: the diff is over, and this line is looked at as any other.
                    self.in_diff = false;
                }
            }
        }
        if t.is_empty() {
            // White space so far (a long run of it is not the start of anything).
            if self.line.len() > MAX_FENCE_INDENT + 1 {
                self.pass(out);
            }
            return;
        }
        if indent > MAX_FENCE_INDENT {
            self.pass(out);
            return;
        }
        // A fence: backticks or tildes, held while it may still grow and then to the end of the line, which is where its info string ends.
        if let Some(fc) = t.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let run = t.chars().take_while(|&c| c == fc).count();
            let all_fence = run == t.chars().count();
            if (all_fence || run >= 3) && t.len() <= MAX_FENCE_LINE {
                return;
            }
            self.pass(out);
            return;
        }
        // A diff header: at the start of the line, held to its end.
        if indent == 0 && may_be_header(&self.line) {
            return;
        }
        self.pass(out);
    }

    /// The line is prose: the header that waited, then what has been written of the line, and the rest of it as it comes.
    fn pass(&mut self, out: &mut String) {
        self.release_held(out);
        out.push_str(&self.line);
        self.line.clear();
        self.mode = LineMode::Pass;
    }

    /// The line break: the line is complete, and what it is is known.
    fn end_line(&mut self, out: &mut String) {
        let mode = std::mem::take(&mut self.mode);
        let line = std::mem::take(&mut self.line);
        match mode {
            LineMode::Pass => {
                out.push('\n');
                return;
            }
            LineMode::Drop => return,
            LineMode::Undecided => {}
        }
        if let Some(open) = self.fence {
            // A line of the block; the closing line ends it.
            if closes_fence(&line, open) {
                self.fence = None;
            }
            return;
        }
        if let Some(open) = fence_opening(&line) {
            self.release_held(out);
            self.fence = Some(open);
            self.in_diff = false;
            self.say_cue(out);
            return;
        }
        if is_diff_header(&line) {
            if self.in_diff {
                return;
            }
            if self.held_header.take().is_some() {
                // Two headers in a row: a diff, and both are code.
                self.in_diff = true;
                self.say_cue(out);
            } else {
                self.held_header = Some(format!("{line}\n"));
            }
            return;
        }
        // Prose (an empty line is prose too, and ends a diff).
        self.release_held(out);
        if self.in_diff && is_diff_body(&line) {
            return;
        }
        self.in_diff = false;
        out.push_str(&line);
        out.push('\n');
    }
}

/// [`for_speech`] for text that is not code: fenced blocks and diffs are taken out first, and [`CODE_CUE`] is said once in their place.
pub fn for_speech_code_aware(text: &str) -> String {
    let mut filter = CodeFilter::default();
    let mut kept = filter.push(text);
    kept.push_str(&filter.finish());
    for_speech(&kept)
}

/// How many characters at the end of `text` could be the start of one of `tags`: the longest such, never the whole tag.
fn partial_tag_len(text: &str, tags: &[&str]) -> usize {
    // The tags are ASCII, so every cut through them is on a character boundary of `text` too.
    tags.iter().filter_map(|tag| (1..tag.len()).rev().find(|&k| text.ends_with(&tag[..k]))).max().unwrap_or(0)
}

/// The text as it is spoken (the written answer is shown as it is): each line without the mark of a list or a heading, the marks of the
/// writing (`*`, backticks) left out, a link said by its words and an address by its site ("example.com", not "h t t p s slash slash"),
/// degrees spelled out, runs of white space made one space, and a line that does not end in a stop or a comma given a stop, so that the
/// speech pauses where the list item ends.
pub fn for_speech(text: &str) -> String {
    speakable(text)
}

/// The text before the list of sources an answer ends with ("Sources:" and its lines): the list is for the screen.
fn without_source_list(text: &str) -> &str {
    match text.find("\n\nSources:\n") {
        Some(i) => &text[..i],
        None => text,
    }
}

/// A line without citation marks (`[1]`, `[2, 3]`): the numbers are for reading, not for saying.
fn without_citations(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find('[') {
        match rest[i..].find(']') {
            Some(j) if j > 1 && rest[i + 1..i + j].chars().all(|c| c.is_ascii_digit() || c == ',' || c == ' ') && rest[i + 1..i + j].chars().any(|c| c.is_ascii_digit()) => {
                out.push_str(rest[..i].trim_end_matches(' '));
                rest = &rest[i + j + 1..];
            }
            _ => {
                out.push_str(&rest[..i + 1]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn speakable(text: &str) -> String {
    let mut out = String::new();
    for line in without_source_list(text).lines() {
        let plain = spoken_links(&without_marks(&without_citations(line))).replace("\u{b0}C", " degrees Celsius").replace("\u{b0}F", " degrees Fahrenheit").replace('\u{b0}', " degrees");
        let plain = strip_list_marker(strip_heading(plain.trim()));
        let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        if plain.is_empty() {
            continue;
        }
        if !out.is_empty() {
            if !out.ends_with(['.', '!', '?', ':', ';', ',', '\u{2026}']) {
                out.push('.');
            }
            out.push(' ');
        }
        out.push_str(&plain);
    }
    out
}

/// A line without the marks of the writing: backticks, and asterisks used for emphasis. An asterisk between two numbers or glued to a letter
/// and a digit is a multiplication ("6*7", "5 * 3") and is said as "times".
fn without_marks(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '`' => {}
            '*' => {
                let prev = i.checked_sub(1).map(|j| chars[j]);
                let next = chars.get(i + 1).copied();
                let glued = prev.is_some_and(char::is_alphanumeric) && next.is_some_and(char::is_alphanumeric);
                let digit_before = chars[..i].iter().rev().find(|c| !c.is_whitespace()).is_some_and(char::is_ascii_digit);
                let digit_after = chars[i + 1..].iter().find(|c| !c.is_whitespace()).is_some_and(char::is_ascii_digit);
                let spaced = prev.is_some_and(char::is_whitespace) && next.is_some_and(char::is_whitespace) && digit_before && digit_after;
                if glued || spaced {
                    out.push_str(" times ");
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Links and addresses in one line as they are said: `[the page](https://example.com/a)` as "the page", and a bare address as its site.
fn spoken_links(line: &str) -> String {
    let mut line = line.to_string();
    // A markdown link keeps its words: take out the "(address)" and the "[" before the "]".
    let mut from = 0;
    while let Some(found) = line[from..].find("](") {
        let close = from + found;
        let Some(end) = line[close..].find(')') else { break };
        let open = line[..close].rfind('[');
        line.replace_range(close..close + end + 1, "");
        match open {
            Some(open) => {
                line.remove(open);
                from = close - 1;
            }
            None => from = close,
        }
    }
    line.split(' ')
        .map(|word| {
            let lead = word.len() - word.trim_start_matches(['(', '"', '\'', '<']).len();
            let rest = &word[lead..];
            let lower = rest.to_ascii_lowercase();
            if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.")) {
                return word.to_string();
            }
            let tail_len = rest.len() - rest.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'', '>']).len();
            let address = &rest[..rest.len() - tail_len];
            let without_scheme = address.split_once("://").map_or(address, |(_, after)| after);
            let host = without_scheme.split(['/', '?', '#']).next().unwrap_or("");
            let host = host.strip_prefix("www.").unwrap_or(host);
            format!("{}{host}{}", &word[..lead], &rest[rest.len() - tail_len..])
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A line without the "#" marks of a heading ("## Title"); a "#1" or "C#" is not one.
fn strip_heading(line: &str) -> &str {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    match line[hashes..].strip_prefix(' ') {
        Some(rest) if (1..=6).contains(&hashes) => rest.trim_start(),
        _ => line,
    }
}

/// A line without the "- ", "\u{2022} " or "3. " that starts an item of a list.
fn strip_list_marker(line: &str) -> &str {
    let mut chars = line.chars();
    if let Some('-' | '\u{2022}' | '\u{2013}') = chars.next() {
        if chars.as_str().starts_with(' ') {
            return chars.as_str().trim_start();
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let after = &line[digits..];
        if after.starts_with(". ") || after.starts_with(") ") {
            return after[2..].trim_start();
        }
    }
    line
}

/// Where sentences end in `text`, as byte indices just after the end (and any closing quote or bracket). A stop at the very end of
/// the text is not counted unless `at_end_counts`: the next character may turn it into part of "3.5". A line break ends a piece too.
fn sentence_ends(text: &str, at_end_counts: bool) -> Vec<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut ends: Vec<usize> = Vec::new();
    let mut k = 0;
    while k < chars.len() {
        let (i, c) = chars[k];
        if c == '\n' {
            if !text[..i].trim().is_empty() && ends.last().is_none_or(|&last| last < i) {
                ends.push(i + 1);
            }
            k += 1;
            continue;
        }
        if !matches!(c, '.' | '!' | '?' | '\u{2026}' | '\u{0964}') {
            k += 1;
            continue;
        }
        // The whole run of stops and closing quotes or brackets.
        let mut j = k;
        while j + 1 < chars.len() && matches!(chars[j + 1].1, '.' | '!' | '?' | '\u{2026}' | '\u{0964}' | '"' | '\'' | ')' | ']' | '\u{201d}' | '\u{2019}') {
            j += 1;
        }
        let end_byte = chars.get(j + 1).map_or(text.len(), |&(b, _)| b);
        let single_stop = c == '.' && j == k;
        let confirmed = match chars.get(j + 1) {
            Some(&(_, next)) => next.is_whitespace(),
            None => at_end_counts,
        };
        if confirmed && !(single_stop && continues_the_sentence(&text[..i])) {
            ends.push(end_byte);
        }
        k = j + 1;
    }
    ends
}

/// Does a full stop after `before` leave the sentence going: an abbreviation, an initial, or the number of a list item?
fn continues_the_sentence(before: &str) -> bool {
    // Just after the last white space, whatever its width in bytes (a no-break space is two, a narrow one three: `rfind` gives the first).
    let word_start = before.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
    let word = before[word_start..].trim_start_matches(['(', '"', '\'', '[']);
    if ABBREVIATIONS.contains(&word.to_lowercase().as_str()) {
        return true;
    }
    // An initial: one capital letter ("J. Smith").
    let mut letters = word.chars();
    if let (Some(first), None) = (letters.next(), letters.next()) {
        if first.is_alphabetic() && first.is_uppercase() {
            return true;
        }
    }
    // The number of a list item, at the start of a line ("1. First"); a number that ends a sentence ("the answer is 3.") is not.
    if !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) {
        let line_start = before[..word_start].trim_end_matches([' ', '\t']);
        return line_start.is_empty() || line_start.ends_with('\n');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(pieces: &[&str]) -> Vec<String> {
        let mut splitter = SentenceSplitter::new();
        let mut out = Vec::new();
        for piece in pieces {
            out.extend(splitter.push(piece));
        }
        out.extend(splitter.finish(false));
        out
    }

    fn by_character(text: &str) -> Vec<String> {
        let chars: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = chars.iter().map(String::as_str).collect();
        stream(&refs)
    }

    #[test]
    fn a_short_first_sentence_stays_whole_and_a_long_one_is_released_after_about_fifteen_words() {
        assert_eq!(pieces_of("Hello, how can I help you today?"), ["Hello, how can I help you today?"]);
        assert_eq!(pieces_of("Yes."), ["Yes."]);
        assert_eq!(pieces_of("Sure, I can help you with that. First, let's look at the settings."), ["Sure, I can help you with that.", "First, let's look at the settings."]);
        let long = "Sure, I can help you with that because there are several things we need to consider before we change anything at all.";
        let pieces = pieces_of(long);
        assert!(pieces.len() >= 2, "{pieces:?}");
        let words = pieces[0].split_whitespace().count();
        assert!((10..=15).contains(&words), "{pieces:?}");
        assert!(!pieces[0].contains("consider"), "{pieces:?}");
        assert_eq!(pieces.join(" ").split_whitespace().count(), long.split_whitespace().count());
        let mut splitter = SentenceSplitter::new();
        assert!(splitter.push("Sure, I can help you with").is_empty(), "nine words is not enough to cut a sentence that has not ended");
        let early = splitter.push(" that because there are several things we need to consider before we change anything.");
        assert_eq!(early.len(), 1, "{early:?}");
        assert!((10..=15).contains(&early[0].split_whitespace().count()), "{early:?}");
    }

    #[test]
    fn the_first_piece_is_a_sentence_of_25_characters_or_more() {
        assert_eq!(pieces_of("Yes. I can help with that today."), ["Yes. I can help with that today."], "a short first sentence is joined to the next");
        assert_eq!(pieces_of("The weather in Pune is sunny. It is 34 degrees."), ["The weather in Pune is sunny.", "It is 34 degrees."]);
    }

    #[test]
    fn later_pieces_are_60_characters_or_more_so_short_sentences_are_joined() {
        let text = "Here is the first part of the answer. One. Two. Three. Four. Five. Six. Seven. Eight. Nine. Ten. Eleven. Twelve.";
        let pieces = pieces_of(text);
        assert_eq!(pieces[0], "Here is the first part of the answer.");
        assert!(pieces[1..pieces.len() - 1].iter().all(|p| p.chars().count() >= LATER_PIECE_MIN), "{pieces:?}");
        assert_eq!(pieces.join(" "), text, "nothing is lost or changed");
    }

    #[test]
    fn a_stop_inside_a_number_an_abbreviation_or_an_initial_is_not_a_sentence_end() {
        assert_eq!(pieces_of("It is 3.5 degrees warmer than yesterday. Tomorrow will be cooler."), ["It is 3.5 degrees warmer than yesterday.", "Tomorrow will be cooler."]);
        assert_eq!(
            pieces_of("Dr. Smith will see you at 5 p.m. tomorrow. Please bring your insurance card with you."),
            ["Dr. Smith will see you at 5 p.m. tomorrow.", "Please bring your insurance card with you."]
        );
        assert_eq!(pieces_of("That was written by J. K. Rowling in the nineties. It sold well."), ["That was written by J. K. Rowling in the nineties.", "It sold well."]);
        assert_eq!(pieces_of("Visit example.com for details about the offer. It ends soon."), ["Visit example.com for details about the offer.", "It ends soon."]);
        assert_eq!(pieces_of("Use e.g. a spoon, or i.e. something small, to stir it well. Then wait."), ["Use e.g. a spoon, or i.e. something small, to stir it well.", "Then wait."]);
    }

    #[test]
    fn a_number_that_ends_a_sentence_is_a_sentence_end_but_the_number_of_a_list_item_is_not() {
        assert_eq!(pieces_of("The answer to your question is 3. That is all there is to it."), ["The answer to your question is 3.", "That is all there is to it."]);
    }

    #[test]
    fn the_pieces_are_the_same_however_the_text_arrives() {
        let text = "The weather in Pune is sunny today. It is 34.5 degrees, with 40% humidity and a light breeze from the west. Have a nice day!";
        let whole = pieces_of(text);
        assert_eq!(whole, ["The weather in Pune is sunny today.", "It is 34.5 degrees, with 40% humidity and a light breeze from the west.", "Have a nice day!"]);
        assert_eq!(by_character(text), whole);
        assert_eq!(stream(&["The weather in Pune is sunny today. It is 34", ".5 degrees, with 40% humidity and a light breeze ", "from the west. Have a nice day!"]), whole);
    }

    #[test]
    fn a_stop_at_the_end_of_what_has_arrived_waits_for_the_next_character() {
        let mut splitter = SentenceSplitter::new();
        assert!(splitter.push("The answer to your question is 3.").is_empty(), "this may be the start of 3.5");
        assert_eq!(splitter.push("5 degrees. And then"), ["The answer to your question is 3.5 degrees."]);
        assert_eq!(splitter.finish(false).as_deref(), Some("And then"));
    }

    #[test]
    fn thinking_is_not_spoken_even_when_the_tags_arrive_in_pieces() {
        assert_eq!(pieces_of("<think>let me work this out</think>Hello there, my dear friend."), ["Hello there, my dear friend."]);
        assert_eq!(pieces_of("<think>\n\n</think>\n\nHello there, my dear friend."), ["Hello there, my dear friend."], "the empty block Qwen3 writes when thinking is off");
        assert_eq!(stream(&["<thi", "nk>hm", "m</thi", "nk>Hello there, my dear friend."]), ["Hello there, my dear friend."]);
        assert_eq!(by_character("<think>hmm</think>Hello there, my dear friend. Second part of it."), ["Hello there, my dear friend.", "Second part of it."]);
        assert_eq!(pieces_of("Before the tag, which is long enough. <think>secret</think>After it, which is long enough as well."), ["Before the tag, which is long enough.", "After it, which is long enough as well."]);
        assert_eq!(pieces_of("<reasoning>why</reasoning>Hello there, my dear friend."), ["Hello there, my dear friend."], "the other tags the engine strips");
    }

    #[test]
    fn thinking_that_never_ends_is_never_spoken() {
        assert!(pieces_of("<think>and then it goes on and on without ever closing the tag at all.").is_empty());
    }

    #[test]
    fn a_less_than_sign_that_is_not_a_tag_is_spoken() {
        let text = "Remember that 3 < 5 is true, and that 5 < 9 is true as well.";
        let pieces = pieces_of(text);
        assert_eq!(pieces.join(" "), text, "a long first sentence is released at a word boundary, and nothing is lost");
        assert!(pieces[0].split_whitespace().count() <= FIRST_EARLY_MAX_WORDS, "{pieces:?}");
        assert_eq!(by_character(text), pieces);
        let mut splitter = SentenceSplitter::new();
        assert!(splitter.push("The text ends with a bracket <").is_empty());
        assert_eq!(splitter.finish(false).as_deref(), Some("The text ends with a bracket <"), "held back as a possible tag, then given back");
    }

    #[test]
    fn an_answer_cut_off_at_the_token_limit_drops_its_unfinished_sentence() {
        // A first sentence of its own, and a second one that was cut off.
        let mut splitter = SentenceSplitter::new();
        assert_eq!(splitter.push("The first sentence is long enough. And this one is cut o"), ["The first sentence is long enough."]);
        assert_eq!(splitter.finish(true), None, "the unfinished one is dropped, as the written answer drops it");
        // A complete sentence that is too short to have been spoken yet is kept, the unfinished one after it is not.
        let mut short = SentenceSplitter::new();
        assert!(short.push("First sentence is here. And this one is cut o").is_empty());
        assert_eq!(short.finish(true).as_deref(), Some("First sentence is here."));
        // Nothing said yet and no complete sentence: that is all there is (the written answer is kept whole in that case too).
        let mut only = SentenceSplitter::new();
        assert!(only.push("A sentence that never gets its end").is_empty());
        assert_eq!(only.finish(true).as_deref(), Some("A sentence that never gets its end"));
        let mut whole = SentenceSplitter::new();
        whole.push("A sentence that never gets its end");
        assert_eq!(whole.finish(false).as_deref(), Some("A sentence that never gets its end"));
    }

    #[test]
    fn line_breaks_end_a_piece_and_the_marks_of_the_writing_are_not_spoken() {
        assert_eq!(
            pieces_of("Here are two things to try today\n- First, restart the router now\n- Second, check the cable"),
            ["Here are two things to try today", "First, restart the router now. Second, check the cable"]
        );
        assert_eq!(pieces_of("This is **very** important, so `listen` carefully to what follows."), ["This is very important, so listen carefully to what follows."]);
        assert_eq!(pieces_of("1. Open the settings and look for the network tab.\n2. Choose the wireless connection."), ["Open the settings and look for the network tab.", "Choose the wireless connection."]);
        assert_eq!(pieces_of("## A heading that is long enough\nAnd the text that follows it, which is long enough too."), ["A heading that is long enough", "And the text that follows it, which is long enough too."]);
        let plain = "It is -5 degrees today, and C# is a language, and #1 is a number.";
        assert_eq!(pieces_of(plain), [plain], "a dash or a hash that is part of the words stays");
    }

    #[test]
    fn a_long_run_with_no_sentence_end_is_cut_at_a_comma_or_a_space() {
        let text = format!("{} and so it goes on and on without a single full stop at all", "word, ".repeat(40));
        let pieces = pieces_of(&text);
        assert!(pieces.len() >= 2, "{pieces:?}");
        assert!(pieces.iter().all(|p| p.chars().count() <= LONGEST_RUN + 10), "{pieces:?}");
        assert_eq!(pieces.join(" ").split_whitespace().count(), text.split_whitespace().count(), "no word is lost");
        let no_commas = "word ".repeat(80);
        let pieces = pieces_of(&no_commas);
        assert!(pieces.len() >= 2 && pieces.iter().all(|p| p.chars().count() <= LONGEST_RUN + 10), "{pieces:?}");
    }

    #[test]
    fn accented_and_non_latin_text_is_cut_on_character_boundaries() {
        let text = "Il fait très chaud à Pune aujourd'hui, mais demain sera plus frais. Prenez un chapeau, s'il vous plaît.";
        assert_eq!(by_character(text), pieces_of(text));
        assert_eq!(pieces_of(text).len(), 2);
        let hindi = "आज पुणे में मौसम बहुत अच्छा है। कल थोड़ी बारिश हो सकती है।";
        assert_eq!(by_character(hindi), pieces_of(hindi), "the Devanagari full stop ends a sentence, and nothing is broken in the middle of a character");
    }

    #[test]
    fn links_degrees_and_marks_are_said_the_way_they_are_heard() {
        assert_eq!(for_speech("Visit https://www.example.com/docs/page?id=3 for details."), "Visit example.com for details.");
        assert_eq!(for_speech("See (https://news.bbc.co.uk/world), or www.python.org."), "See (news.bbc.co.uk), or python.org.");
        assert_eq!(for_speech("Read [the BBC story](https://www.bbc.com/news/1234) first."), "Read the BBC story first.");
        assert_eq!(for_speech("Two links: [one](https://a.com/x) and [two](https://b.com/y)."), "Two links: one and two.");
        assert_eq!(for_speech("It is 24\u{b0}C now, or 75\u{b0}F, and the angle is 90\u{b0}."), "It is 24 degrees Celsius now, or 75 degrees Fahrenheit, and the angle is 90 degrees.");
        assert_eq!(for_speech("The word http is not an address, and neither is www"), "The word http is not an address, and neither is www");
        assert_eq!(for_speech("Email bob@example.com or call 555-1234."), "Email bob@example.com or call 555-1234.");
        assert_eq!(for_speech("A [broken link](https://a.com/x and a lone ](bracket."), "A [broken link](https://a.com/x and a lone ](bracket.", "an unfinished link is left as it is");
        // The pieces of a streamed answer are cleaned the same way.
        assert_eq!(pieces_of("You can read more at https://www.example.com/long/path/here. It is a good page."), ["You can read more at example.com.", "It is a good page."]);
    }

    /// Found in a review: `rfind` gives the first byte of a white space, and `+ 1` lands inside a no-break space (2 bytes), a narrow one (3) or an
    /// ideographic one (3), which panicked at the full stop after "7:30 PM" written with such a space (Google and web pages write them).
    #[test]
    fn a_wide_white_space_before_the_last_word_of_a_sentence_does_not_panic() {
        for space in ['\u{a0}', '\u{202f}', '\u{2009}', '\u{3000}', '\u{2003}', '\u{85}'] {
            let text = format!("The game starts at 7:30{space}PM. Bring a jacket, it will be cold and windy tonight.");
            assert_eq!(pieces_of(&text), ["The game starts at 7:30 PM.", "Bring a jacket, it will be cold and windy tonight."], "{space:?}");
            assert_eq!(by_character(&text), pieces_of(&text), "{space:?}, character by character");
            let list = format!("Steps follow.\n1.{space}Open it.\n2.{space}Close it.");
            let _ = pieces_of(&list);
            let initials = format!("It was written by J.{space}K. Rowling{space}in the nineties. It sold well.");
            assert_eq!(pieces_of(&initials).len(), 2, "{space:?}");
        }
        assert_eq!(pieces_of("The next shop is about 5\u{a0}km. That is far away from here, by any road you could take."), ["The next shop is about 5 km.", "That is far away from here, by any road you could take."]);
    }

    /// Random text from a pool of awkward pieces (wide spaces, accents, emoji, tags, marks, stops, quotes, digits): nothing may panic, however the text
    /// is cut into pieces as it arrives, and every way of cutting it gives the same pieces.
    #[test]
    fn random_awkward_text_never_panics_and_is_cut_the_same_however_it_arrives() {
        const POOL: &[&str] = &[
            "Hello", " ", "  ", "world", ".", "!", "?", "\n", "\u{a0}", "\u{202f}", "\u{3000}", "7:30", "PM", "Dr.", "e.g.", "3.5", "1.", "J.", "\u{e9}", "e\u{301}", "\u{1f600}", "\u{0964}", "\u{2026}",
            "\"", "'", ")", "(", "[", "]", "*", "**", "`", "#", "- ", "<think>", "</think>", "<", "</", "<thi", "http://a.com/x", "www.b.org", "](", "\u{b0}C", "%", ",", ";", ":", "and", "the", "\t", "\r\n",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..4000 {
            let text: String = (0..1 + next(40)).map(|_| POOL[next(POOL.len())]).collect();
            let whole = pieces_of(&text);
            assert_eq!(by_character(&text), whole, "character by character: {text:?}");
            // Cut at random places (on character boundaries).
            let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
            let mut cuts: Vec<usize> = (0..next(4)).map(|_| boundaries[next(boundaries.len())]).collect();
            cuts.sort_unstable();
            let mut parts: Vec<&str> = Vec::new();
            let mut at = 0;
            for cut in cuts {
                parts.push(&text[at..cut]);
                at = cut;
            }
            parts.push(&text[at..]);
            assert_eq!(stream(&parts), whole, "cut as {parts:?}");
            // A cut-off ending never panics either.
            let mut splitter = SentenceSplitter::new();
            splitter.push(&text);
            let _ = splitter.finish(true);
        }
    }

    #[test]
    fn the_numbers_of_citations_and_the_list_of_sources_are_not_said_aloud() {
        let answer = "Rust 1.90.0 made lld the default linker [1]. It is faster [2, 3] on Linux.

Sources:
[1] blog.rust-lang.org, 2025-09-14: Announcing Rust 1.90.0
    https://blog.rust-lang.org/a";
        assert_eq!(for_speech(answer), "Rust 1.90.0 made lld the default linker. It is faster on Linux.");
        // Brackets that are not a citation stay: a list index in a text, an optional word.
        assert_eq!(for_speech("Use a[i] and [optional] words."), "Use a[i] and [optional] words.");
        assert_eq!(for_speech("Item [a1] and [] stay."), "Item [a1] and [] stay.");
    }

    #[test]
    fn a_star_between_numbers_is_a_multiplication_and_the_other_stars_are_marks() {
        assert_eq!(for_speech("6*7 equals 42."), "6 times 7 equals 42.");
        assert_eq!(for_speech("What is 5 * 3? It is 15."), "What is 5 times 3? It is 15.");
        assert_eq!(for_speech("Use x*2 here."), "Use x times 2 here.");
        assert_eq!(for_speech("This is **very** important and *so* is that*."), "This is very important and so is that.");
        assert_eq!(for_speech("* one\n* two"), "one. two");
    }

    #[test]
    fn nothing_is_made_of_empty_text() {
        assert!(pieces_of("").is_empty());
        assert!(pieces_of("  \n ").is_empty());
        assert!(pieces_of("<think>only thinking</think>").is_empty());
    }

    // --- Code is shown, not spoken (card OA10-06, UX-08) ---

    fn code_stream(pieces: &[&str]) -> Vec<String> {
        let mut splitter = SentenceSplitter::code_aware();
        let mut out = Vec::new();
        for piece in pieces {
            out.extend(splitter.push(piece));
        }
        out.extend(splitter.finish(false));
        out
    }

    fn code_by_character(text: &str) -> Vec<String> {
        let chars: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = chars.iter().map(String::as_str).collect();
        code_stream(&refs)
    }

    fn cues(pieces: &[String]) -> usize {
        pieces.join(" ").matches(CODE_CUE).count()
    }

    const ANSWER_WITH_CODE: &str = "Here is how to parse it, it is short and clear.\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\nThat prints a greeting, which is all it does.";

    #[test]
    fn fenced_code_is_not_spoken() {
        let said = pieces_of_code_aware(ANSWER_WITH_CODE);
        let all = said.join(" ");
        assert!(!all.contains("fn main") && !all.contains("println") && !all.contains("hello"), "{said:?}");
        assert!(all.contains("Here is how to parse it") && all.contains("That prints a greeting"), "the words around the code are said: {said:?}");
        assert_eq!(cues(&said), 1, "{said:?}");
        assert!(all.find(CODE_CUE).unwrap() > all.find("Here is how").unwrap() && all.find(CODE_CUE).unwrap() < all.find("That prints").unwrap(), "the cue stands where the code was: {said:?}");
        // The splitter that is not code aware says everything, as it always did: the new behaviour is for the turn that asks for it.
        assert!(pieces_of(ANSWER_WITH_CODE).join(" ").contains("println"));
        assert!(for_speech(ANSWER_WITH_CODE).contains("println"));
        // The whole text of an answer that was not streamed.
        let whole = for_speech_code_aware(ANSWER_WITH_CODE);
        assert!(!whole.contains("println") && whole.contains(CODE_CUE) && whole.contains("That prints a greeting"), "{whole}");
    }

    #[test]
    fn fence_split_across_pieces() {
        let text = "Look at this one, which is short:\n```rust\nlet x = 1;\nlet y = 2;\n```\nThat adds nothing, which is the point of it.";
        let whole = pieces_of_code_aware(text);
        assert!(!whole.join(" ").contains("let x"), "{whole:?}");
        assert_eq!(cues(&whole), 1);
        assert_eq!(code_by_character(text), whole, "one character at a time");
        assert_eq!(code_stream(&["Look at this one, which is short:\n``", "`rust\nlet x = 1;\nlet y = 2;\n``", "`\nThat adds nothing, which is the point of it."]), whole, "a fence cut in the middle of its three characters");
        assert_eq!(code_stream(&["Look at this one, which is short:\n```ru", "st\nlet x = 1;\nlet y = 2;\n```", "\nThat adds nothing, which is the point of it."]), whole, "and in its info string");
        // The text before the fence is not held back while the fence is being written: the first piece is out before the block is closed.
        let mut splitter = SentenceSplitter::code_aware();
        let early = splitter.push("Look at this one, which is short:\n```ru");
        assert_eq!(early, ["Look at this one, which is short:"], "{early:?}");
    }

    #[test]
    fn unterminated_fence_is_dropped_silently() {
        let said = pieces_of_code_aware("Here it is, and it is short.\n```python\nprint(1)\nprint(2)");
        assert!(!said.join(" ").contains("print"), "{said:?}");
        assert_eq!(cues(&said), 1, "the cue was said where the block began, and nothing more at the end: {said:?}");
        assert_eq!(said, ["Here it is, and it is short.", CODE_CUE], "{said:?}");
        // A fence line with no line break after it at the end of the text.
        assert_eq!(cues(&pieces_of_code_aware("Here it is, and it is short.\n```python")), 1);
        // A block cut off at the token limit: nothing of it is spoken either, only the cue that stood in its place.
        let mut splitter = SentenceSplitter::code_aware();
        assert_eq!(splitter.push("Here it is, and it is short.\n```python\nprint(1)\nprin"), ["Here it is, and it is short."]);
        assert_eq!(splitter.finish(true).as_deref(), Some(CODE_CUE));
    }

    #[test]
    fn cue_is_said_once() {
        let two = "First the setup, which is short.\n```sh\ncargo build\n```\nThen the test, which is short too.\n```sh\ncargo test\n```\nThat is all of it, as far as I know.";
        let said = pieces_of_code_aware(two);
        assert_eq!(cues(&said), 1, "{said:?}");
        let all = said.join(" ");
        assert!(!all.contains("cargo") && all.contains("Then the test") && all.contains("That is all of it"), "{said:?}");
        // A new answer is a new splitter, which says it again.
        assert_eq!(cues(&pieces_of_code_aware(two)), 1);
    }

    #[test]
    fn an_answer_that_is_only_code_is_only_the_cue() {
        assert_eq!(pieces_of_code_aware("```rust\nfn main() {}\n```"), [CODE_CUE]);
        assert_eq!(pieces_of_code_aware("```\nline one\n```\n\n```\nline two\n```\n"), [CODE_CUE]);
        assert_eq!(code_by_character("```rust\nfn main() {}\n```"), [CODE_CUE]);
    }

    #[test]
    fn a_longer_fence_is_closed_by_a_fence_as_long_and_tildes_and_indents_are_fences_too() {
        // A block that shows a block: the three backticks inside do not end the four of the outside.
        let nested = "Here is the template, which is short.\n````markdown\n```rust\nfn inner() {}\n```\nstill code\n````\nThat is the template, which is the end.";
        let said = pieces_of_code_aware(nested);
        let all = said.join(" ");
        assert!(!all.contains("inner") && !all.contains("still code") && all.contains("That is the template"), "{said:?}");
        let tildes = "Here is a block with tildes, which is short.\n~~~python\nprint('x')\n~~~\nThat was the block, which is the end of it.";
        assert!(!pieces_of_code_aware(tildes).join(" ").contains("print"));
        // A block in a list is indented; a backtick fence does not close on a tilde line, and a fence that is shorter does not close a longer one.
        let list = "Two steps follow, which are short.\n1. Build it:\n   ```sh\n   cargo build\n   ```\n2. Test it.\nThat is all of it, as far as I know.";
        let all = pieces_of_code_aware(list).join(" ");
        assert!(!all.contains("cargo") && all.contains("Build it") && all.contains("Test it"), "{all}");
        let mixed = "A block follows, which is short.\n```\nbody\n~~~\nstill body\n```\nAfter it comes this, which is the end.";
        let all = pieces_of_code_aware(mixed).join(" ");
        assert!(!all.contains("body") && all.contains("After it comes this"), "{all}");
        // Inline code is spoken as the words it is, with no backticks, and one backtick fence on a line is not a block.
        let inline = "Call `parse_strict` with the text, which is short.\nOr write ```x``` inline, which is rare.";
        assert_eq!(cues(&pieces_of_code_aware(inline)), 0);
        assert!(pieces_of_code_aware(inline).join(" ").contains("parse_strict"));
    }

    #[test]
    fn thinking_around_a_fence_is_still_not_spoken_and_a_fence_inside_thinking_says_nothing() {
        let said = pieces_of_code_aware("<think>\n```\nhidden\n```\n</think>Hello there, my dear friend.");
        assert_eq!(said, ["Hello there, my dear friend."], "code the model only thought of is not announced: {said:?}");
        let said = pieces_of_code_aware("<think>plan</think>Hello there, my dear friend.\n```\ncode\n```\n");
        assert_eq!(said, ["Hello there, my dear friend.", CODE_CUE], "{said:?}");
        let said = code_stream(&["<thi", "nk>x</thi", "nk>Hello there, my dear friend.\n``", "`\ncode\n``", "`\n"]);
        assert_eq!(said, ["Hello there, my dear friend.", CODE_CUE], "{said:?}");
    }

    #[test]
    fn a_diff_outside_a_fence_is_not_spoken_when_it_has_two_header_lines_and_one_header_line_is_prose() {
        let diff = "I changed the call, which is short.\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -40,3 +40,3 @@ fn load() {\n     let a = 1;\n-    parse(a);\n+    parse_strict(a);\n }\n\nThat is the whole change, which is the end.";
        let said = pieces_of_code_aware(diff);
        let all = said.join(" ");
        assert!(!all.contains("parse") && !all.contains("main.rs") && !all.contains("@@") && all.contains("I changed the call") && all.contains("That is the whole change"), "{said:?}");
        assert_eq!(cues(&said), 1, "{said:?}");
        assert_eq!(code_by_character(diff), said);
        let git = "The patch is below, which is short.\ndiff --git a/x.rs b/x.rs\nindex 83db48f..bf2c3a1 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new\nDone with the patch, which is the end of it.";
        let all = pieces_of_code_aware(git).join(" ");
        assert!(!all.contains("old") && !all.contains("diff --git") && all.contains("Done with the patch"), "{all}");
        // One header alone is not a diff: a rule, a line of prose that starts with the same marks, and a list are said as they always were.
        for prose in ["Here is a rule, which is short.\n---\nAnd then more text follows it, which is short.", "Look here, which is short.\n--- see below for more\nThen it goes on and on, which is short.", "Items follow, which are short.\n- first one\n- second one\n+ third one, which is short."] {
            assert_eq!(cues(&pieces_of_code_aware(prose)), 0, "{prose}");
            assert!(pieces_of_code_aware(prose).join(" ").split_whitespace().count() > 6, "{prose}");
        }
        let said = pieces_of_code_aware("Look here, which is short.\n--- see below for more\nThen it goes on and on, which is short.").join(" ");
        assert!(said.contains("see below for more"), "a header that waited and was not followed by another is said: {said}");
        // A line that only starts like a header is prose at once.
        let mut splitter = SentenceSplitter::code_aware();
        assert_eq!(splitter.push("Index 3 is the one that you want, which is short.\n"), ["Index 3 is the one that you want, which is short."]);
        assert_eq!(code_stream(&["index 3 is the one that you want, which is short."]), ["index 3 is the one that you want, which is short."]);
        assert_eq!(code_stream(&["diffusion is a word, which is short.\ndiffs are not code, which is short."]), ["diffusion is a word, which is short.", "diffs are not code, which is short."]);
    }

    #[test]
    fn a_splitter_that_is_not_code_aware_and_text_with_no_code_are_the_same_either_way() {
        const POOL: &[&str] = &[
            "Hello", " ", "world", ".", "!", "\n", "7:30", "PM", "Dr.", "3.5", "1.", "- ", "* ", "#", "index", "diff", "-", "+", "--", " --- ", "@", "`x`", "\u{e9}", ",", ";", "and", "the", "<think>", "</think>", "\r\n",
        ];
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..3000 {
            let text: String = (0..1 + next(30)).map(|_| POOL[next(POOL.len())]).collect();
            let plain = pieces_of(&text);
            let code = pieces_of_code_aware(&text);
            // Lines that start like a header (a `- ` line is not one, `--` and `+` alone are held and let through) never change what is said, unless two of them come in a row; the
            // text of such a case is left to the other tests.
            let has_marker = text.lines().filter(|l| l.starts_with("--- ") || l.starts_with("+++ ") || l.starts_with("@@ ") || l.starts_with("diff --git ") || l.starts_with("index ")).count() >= 2;
            if !has_marker {
                assert_eq!(code, plain, "{text:?}");
            }
            assert_eq!(code_by_character(&text), code, "{text:?}");
        }
    }

    /// Random text with fences and diff headers in it: nothing panics, and every way of cutting it into pieces gives the same words.
    #[test]
    fn random_code_text_is_spoken_the_same_however_it_arrives() {
        const POOL: &[&str] = &[
            "Hello there, my dear friend.", "\n", "\n", "```", "```rust", "````", "~~~", "~~", "`", "``", "fn main() {}", "    ", "   ```", "--- a/x", "+++ b/x", "@@ -1,2 +1,2 @@", "diff --git a/x b/x", "index 83db48f..bf2c3a1", "-old", "+new", " ctx",
            "\\ No newline", "---", "- item", "\r\n", "<think>", "</think>", "<thi", "\u{a0}", "\u{e9}", "x", " ", ". ", "Done.",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..4000 {
            let text: String = (0..1 + next(30)).map(|_| POOL[next(POOL.len())]).collect();
            let whole = pieces_of_code_aware(&text);
            assert_eq!(code_by_character(&text), whole, "character by character: {text:?}");
            let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
            let mut cuts: Vec<usize> = (0..next(5)).map(|_| boundaries[next(boundaries.len())]).collect();
            cuts.sort_unstable();
            let mut parts: Vec<&str> = Vec::new();
            let mut at = 0;
            for cut in cuts {
                parts.push(&text[at..cut]);
                at = cut;
            }
            parts.push(&text[at..]);
            assert_eq!(code_stream(&parts), whole, "cut as {parts:?}");
            assert!(cues(&whole) <= 1, "{text:?}: {whole:?}");
            let mut splitter = SentenceSplitter::code_aware();
            splitter.push(&text);
            let _ = splitter.finish(true);
        }
    }
}
