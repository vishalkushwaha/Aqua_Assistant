//! A count of tokens that learns from the model server (card OA4-02, task CX-03 of the OpenCode audit).
//!
//! The planner's prompt used to be kept to the window by counting three characters to a token, always. That is about right for English prose, a little generous for JSON and code, and
//! far too generous for text that is not Latin (a Hindi or Chinese page costs a token or two a character). The server says how many tokens each prompt really was (`prompt_n` read plus
//! `cache_n` taken from its cache), so this meter keeps one number, the characters a token costs, for each of three kinds of text and moves it towards what the server measured.
//!
//! * [`Kind::Prose`], [`Kind::Code`] (a tenth or more of the characters are `{}[]()=;:"\`) and [`Kind::NonLatin`] (more than 30 % of the characters are not ASCII);
//! * the number starts at 3.0 (the rule it replaces) for prose and code and 1.5 for text that is not Latin, is moved by an exponential average (weight 0.3 for the newest sample) and
//!   kept between 1.5 and 4.5;
//! * until three samples have been seen, the lower of the measured and the starting number is used: a prompt may be counted as too long, never as too short, on the word of one sample;
//! * a text of mixed scripts is counted by parts: its ASCII characters at the rate of prose or of code, the others at the rate of text that is not Latin.
//!
//! The meter sees lengths and counts only, never the words of a prompt. This module reads no environment, no file and no other module: it is plain numbers, so it can be tested without a
//! model. The process has one meter ([`meter`]); a test that must not move it uses its own ([`TokenMeter::new`]) or [`with_test_meter`].

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

/// What a text is mostly made of, which is what decides how many characters a token takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Words and sentences in a Latin script.
    Prose,
    /// Code, JSON, paths and logs: a lot of punctuation.
    Code,
    /// More than 30 % of the characters are not ASCII (Devanagari, Cyrillic, Chinese, Arabic ...).
    NonLatin,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Prose, Kind::Code, Kind::NonLatin];

    fn index(self) -> usize {
        match self {
            Kind::Prose => 0,
            Kind::Code => 1,
            Kind::NonLatin => 2,
        }
    }

    /// A word for logs and the status.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Prose => "prose",
            Kind::Code => "code",
            Kind::NonLatin => "non_latin",
        }
    }
}

/// Characters a token costs before the server has said anything (hundredths): the rule of three that this meter replaces, and one and a half for text that is not Latin.
const DEFAULT_X100: [u32; 3] = [300, 300, 150];
/// The lowest and the highest number a sample may move the meter to (hundredths).
const MIN_X100: u32 = 150;
const MAX_X100: u32 = 450;
/// The weight of the newest sample, in percent.
const ALPHA_PERCENT: u64 = 30;
/// Samples after which the measured number is trusted even when it is higher than the starting one.
pub const TRUSTED_AFTER: u32 = 3;
/// A prompt shorter than this says little about the cost of a character: the server also counts the few tokens its chat template adds around it.
const MIN_PROMPT_CHARS: usize = 400;

/// The characters of a text, counted by what decides their cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mix {
    ascii: usize,
    other: usize,
    /// Among the ASCII characters, those of `{}[]()=;:"\`.
    symbols: usize,
}

impl Mix {
    fn of(text: &str) -> Mix {
        let mut m = Mix::default();
        for c in text.chars() {
            if c.is_ascii() {
                m.ascii += 1;
                if matches!(c, '{' | '}' | '[' | ']' | '(' | ')' | '=' | ';' | ':' | '"' | '\\') {
                    m.symbols += 1;
                }
            } else {
                m.other += 1;
            }
        }
        m
    }

    fn total(&self) -> usize {
        self.ascii + self.other
    }

    /// The kind of the whole text.
    fn kind(&self) -> Kind {
        let total = self.total();
        if total == 0 {
            Kind::Prose
        } else if self.other * 100 > total * 30 {
            Kind::NonLatin
        } else if self.symbols * 100 > total * 12 {
            Kind::Code
        } else {
            Kind::Prose
        }
    }

    /// The kind of the ASCII part alone.
    fn ascii_kind(&self) -> Kind {
        if self.ascii > 0 && self.symbols * 100 > self.ascii * 12 {
            Kind::Code
        } else {
            Kind::Prose
        }
    }
}

/// What a text is mostly made of: more than 30 % of its characters not ASCII is [`Kind::NonLatin`], more than 12 % of them `{}[]()=;:"\` is [`Kind::Code`], anything else [`Kind::Prose`].
pub fn kind_of(text: &str) -> Kind {
    Mix::of(text).kind()
}

/// Characters a token costs, learned from the server's own counts, for each [`Kind`].
pub struct TokenMeter {
    cpt_x100: [AtomicU32; 3],
    samples: [AtomicU32; 3],
}

impl Default for TokenMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenMeter {
    pub const fn new() -> TokenMeter {
        TokenMeter {
            cpt_x100: [AtomicU32::new(DEFAULT_X100[0]), AtomicU32::new(DEFAULT_X100[1]), AtomicU32::new(DEFAULT_X100[2])],
            samples: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
        }
    }

    /// The characters a token costs now for this kind of text: the starting number with no sample, the lower of that and the measured one with fewer than [`TRUSTED_AFTER`] samples,
    /// the measured one after.
    pub fn chars_per_token(&self, kind: Kind) -> f64 {
        let i = kind.index();
        let measured = self.cpt_x100[i].load(Ordering::Relaxed);
        let x100 = if self.samples[i].load(Ordering::Relaxed) >= TRUSTED_AFTER { measured } else { measured.min(DEFAULT_X100[i]) };
        f64::from(x100) / 100.0
    }

    /// How many samples have moved the number for this kind.
    pub fn samples(&self, kind: Kind) -> u32 {
        self.samples[kind.index()].load(Ordering::Relaxed)
    }

    /// The tokens of a text, rounded up. A text of mixed scripts is counted by parts.
    pub fn tokens(&self, text: &str) -> usize {
        let m = Mix::of(text);
        let ascii = m.ascii as f64 / self.chars_per_token(m.ascii_kind());
        let other = m.other as f64 / self.chars_per_token(Kind::NonLatin);
        (ascii + other).ceil() as usize
    }

    /// The tokens of `chars` characters of this kind of text, rounded up.
    pub fn tokens_for_chars(&self, chars: usize, kind: Kind) -> usize {
        (chars as f64 / self.chars_per_token(kind)).ceil() as usize
    }

    /// The characters of this kind of text that `tokens` tokens hold, rounded down.
    pub fn chars_for(&self, tokens: usize, kind: Kind) -> usize {
        (tokens as f64 * self.chars_per_token(kind)).floor() as usize
    }

    /// The characters that `tokens` tokens hold at the dearer of the two rates for Latin text (prose and code), rounded down. For the room of the rules, the list of tools and the
    /// blocks after them: that part of the prompt is the same text in every round, and its room must not depend on which of the two kinds the last reply happened to teach.
    pub fn latin_chars_for(&self, tokens: usize) -> usize {
        (tokens as f64 * self.chars_per_token(Kind::Prose).min(self.chars_per_token(Kind::Code))).floor() as usize
    }

    /// Takes in what the server counted: `prompt` was `server_tokens` tokens (what it read plus what it took from its cache). Returns the kind that was moved, or `None` when the
    /// sample says little (a short prompt, no count) or cannot be put to one kind (a text of mixed scripts, between 5 % and 30 % not ASCII).
    pub fn observe(&self, prompt: &str, server_tokens: u64) -> Option<Kind> {
        let (kind, chars) = self.attribute(prompt)?;
        if server_tokens == 0 {
            return None;
        }
        let sample = clamp_x100((chars as f64 * 100.0 / server_tokens as f64).round() as i64);
        let i = kind.index();
        if self.samples[i].fetch_add(1, Ordering::Relaxed) == 0 {
            // The first sample is the first thing known: it replaces the starting number instead of being averaged with it.
            self.cpt_x100[i].store(sample, Ordering::Relaxed);
        } else {
            let _ = self.cpt_x100[i].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                let mixed = (u64::from(old) * (100 - ALPHA_PERCENT) + u64::from(sample) * ALPHA_PERCENT) / 100;
                Some(clamp_x100(mixed as i64))
            });
        }
        Some(kind)
    }

    /// Takes in a refusal that gave no count: the prompt was **at least** `at_least_tokens` tokens (the window it did not fit). That bounds the characters a token costs from above, so the
    /// number only goes down. Returns the kind that was moved.
    pub fn observe_at_least(&self, prompt: &str, at_least_tokens: u64) -> Option<Kind> {
        let (kind, chars) = self.attribute(prompt)?;
        if at_least_tokens == 0 {
            return None;
        }
        let bound = clamp_x100((chars as f64 * 100.0 / at_least_tokens as f64).floor() as i64);
        let i = kind.index();
        let _ = self.cpt_x100[i].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| Some(old.min(bound)));
        Some(kind)
    }

    /// Which kind a prompt's sample belongs to, and how many characters it has.
    fn attribute(&self, prompt: &str) -> Option<(Kind, usize)> {
        let m = Mix::of(prompt);
        let total = m.total();
        if total < MIN_PROMPT_CHARS {
            return None;
        }
        // Mostly one script: that kind. A few non-ASCII characters in an English prompt do not change what an ASCII character costs. Between the two, the sample cannot be put to a kind.
        if m.other * 100 > total * 30 {
            Some((Kind::NonLatin, total))
        } else if m.other * 100 <= total * 5 {
            Some((m.ascii_kind(), total))
        } else {
            None
        }
    }

    /// Back to the starting numbers (tests, and a model that was replaced by another with its own tokenizer).
    pub fn reset(&self) {
        for i in 0..3 {
            self.cpt_x100[i].store(DEFAULT_X100[i], Ordering::Relaxed);
            self.samples[i].store(0, Ordering::Relaxed);
        }
    }
}

fn clamp_x100(v: i64) -> u32 {
    v.clamp(i64::from(MIN_X100), i64::from(MAX_X100)) as u32
}

static GLOBAL: OnceLock<TokenMeter> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_METER: std::cell::Cell<Option<&'static TokenMeter>> = const { std::cell::Cell::new(None) };
}

/// The meter of the process: what every prompt of the planner is counted by, and what every reply of the server teaches.
pub fn meter() -> &'static TokenMeter {
    #[cfg(test)]
    {
        if let Some(own) = TEST_METER.with(|m| m.get()) {
            return own;
        }
    }
    GLOBAL.get_or_init(TokenMeter::new)
}

/// Runs `f` with a meter of its own for this thread (fresh, at the starting numbers), so that a test that teaches it cannot move the numbers another test counts by. The meter is given to
/// `f`, and `meter()` returns it on this thread until `f` ends.
#[cfg(test)]
pub fn with_test_meter<R>(f: impl FnOnce(&'static TokenMeter) -> R) -> R {
    struct Restore(Option<&'static TokenMeter>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_METER.with(|m| m.set(self.0));
        }
    }
    // A meter is a few bytes; leaking one per test is what gives it the lifetime `meter()` returns.
    let own: &'static TokenMeter = Box::leak(Box::new(TokenMeter::new()));
    let _restore = Restore(TEST_METER.with(|m| m.replace(Some(own))));
    f(own)
}

/// The number of tokens a refusal says the prompt had, from the sentence the server layer writes for a prompt that did not fit ("... (6144 tokens; the prompt has 6234)").
pub fn refusal_prompt_tokens(message: &str) -> Option<u64> {
    let at = message.find("the prompt has ")? + "the prompt has ".len();
    let digits: String = message[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A text of `n` characters of English-like prose.
    fn prose(n: usize) -> String {
        "the quick brown fox jumps over the lazy dog and then rests a while. ".chars().cycle().take(n).collect()
    }

    fn json(n: usize) -> String {
        "{\"tool\":\"fs.read\",\"arguments\":{\"path\":\"src/a.rs\",\"max\":[1,2]}} ".chars().cycle().take(n).collect()
    }

    fn hindi(n: usize) -> String {
        "यह एक परीक्षा वाक्य है जो हिंदी में लिखा गया है ".chars().cycle().take(n).collect()
    }

    #[test]
    fn a_text_is_prose_code_or_not_latin_by_the_share_of_its_characters() {
        assert_eq!(kind_of(""), Kind::Prose);
        assert_eq!(kind_of(&prose(500)), Kind::Prose);
        assert_eq!(kind_of(&json(500)), Kind::Code);
        assert_eq!(kind_of(&hindi(500)), Kind::NonLatin);
        // 30 % is the line: 30 of 100 is still prose, 31 of 100 is not Latin.
        let edge = |non_ascii: usize| format!("{}{}", "é".repeat(non_ascii), "a".repeat(100 - non_ascii));
        assert_eq!(kind_of(&edge(30)), Kind::Prose);
        assert_eq!(kind_of(&edge(31)), Kind::NonLatin);
        // 12 % punctuation is the line for code.
        let punct = |n: usize| format!("{}{}", "{".repeat(n), "a".repeat(100 - n));
        assert_eq!(kind_of(&punct(12)), Kind::Prose);
        assert_eq!(kind_of(&punct(13)), Kind::Code);
        // Not Latin wins over code.
        assert_eq!(kind_of(&format!("{}{}", "é".repeat(40), "{".repeat(60))), Kind::NonLatin);
    }

    #[test]
    fn with_no_sample_a_character_costs_a_third_of_a_token_and_a_letter_that_is_not_latin_two_thirds() {
        let m = TokenMeter::new();
        assert_eq!((m.chars_per_token(Kind::Prose), m.chars_per_token(Kind::Code), m.chars_per_token(Kind::NonLatin)), (3.0, 3.0, 1.5));
        // The rule it replaces: three characters a token, rounded up (budget::tokens).
        assert_eq!(m.tokens(""), 0);
        assert_eq!(m.tokens("abc"), 1);
        assert_eq!(m.tokens("abcd"), 2);
        assert_eq!(m.tokens(&prose(3000)), crate::modules::context::budget::tokens(&prose(3000)));
        assert_eq!(m.tokens(&json(3000)), 1000);
        // Text that is not Latin costs twice as much.
        assert_eq!(m.tokens(&"क".repeat(300)), 200);
        assert_eq!(m.chars_for(1000, Kind::Prose), 3000);
        assert_eq!(m.tokens_for_chars(3000, Kind::Prose), 1000);
        assert_eq!(m.chars_for(1000, Kind::NonLatin), 1500);
    }

    #[test]
    fn a_text_of_mixed_scripts_is_counted_by_its_parts() {
        let m = TokenMeter::new();
        // 3,000 ASCII characters (1,000 tokens) and 300 Hindi ones (200 tokens): 10 % not Latin, so the whole is prose, and still the Hindi part is not counted at the rate of prose.
        let mixed = format!("{}{}", prose(3000), "क".repeat(300));
        assert_eq!(kind_of(&mixed), Kind::Prose);
        assert_eq!(m.tokens(&mixed), 1200);
        // The same characters counted all at the rate of prose would be 1,100.
        assert_eq!(m.tokens_for_chars(3300, Kind::Prose), 1100);
    }

    #[test]
    fn the_first_sample_replaces_the_starting_number_and_the_next_ones_are_averaged_in() {
        let m = TokenMeter::new();
        // 3,000 characters that the server counted as 1,000 tokens: 3.0 again. Then 2,000 tokens: 1.5.
        assert_eq!(m.observe(&prose(3000), 750), Some(Kind::Prose));
        assert_eq!(m.chars_per_token(Kind::Prose), 3.0, "4.0 measured, but fewer than three samples: the lower of the two is used");
        assert_eq!(m.samples(Kind::Prose), 1);
        // The second: 0.7 * 4.00 + 0.3 * 2.00 = 3.40.
        m.observe(&prose(3000), 1500);
        assert_eq!(m.cpt_x100[0].load(Ordering::Relaxed), 340);
        assert_eq!(m.samples(Kind::Prose), 2);
        // Other kinds were not touched.
        assert_eq!((m.samples(Kind::Code), m.chars_per_token(Kind::Code)), (0, 3.0));
    }

    #[test]
    fn until_three_samples_the_lower_of_the_measured_and_the_starting_number_is_used() {
        let m = TokenMeter::new();
        // Prose that the server counts at 4.0 characters a token: three samples before 4.0 is believed.
        for _ in 0..2 {
            m.observe(&prose(4000), 1000);
            assert_eq!(m.chars_per_token(Kind::Prose), 3.0);
        }
        m.observe(&prose(4000), 1000);
        assert_eq!(m.chars_per_token(Kind::Prose), 4.0, "and then it is believed");
        // A lower number is believed at once.
        let n = TokenMeter::new();
        n.observe(&json(3000), 1500);
        assert_eq!(n.chars_per_token(Kind::Code), 2.0);
    }

    #[test]
    fn the_number_stays_between_one_and_a_half_and_four_and_a_half() {
        let m = TokenMeter::new();
        for _ in 0..5 {
            m.observe(&prose(4000), 100); // 40 characters a token
        }
        assert_eq!(m.chars_per_token(Kind::Prose), 4.5);
        let n = TokenMeter::new();
        for _ in 0..5 {
            n.observe(&prose(4000), 4000); // one character a token
        }
        assert_eq!(n.chars_per_token(Kind::Prose), 1.5);
    }

    #[test]
    fn a_short_prompt_a_zero_count_and_a_text_of_mixed_scripts_teach_nothing_and_a_text_that_is_not_latin_teaches_its_own_kind() {
        let m = TokenMeter::new();
        assert_eq!(m.observe(&prose(300), 100), None, "the chat template's few tokens are too much of a short prompt");
        assert_eq!(m.observe(&prose(3000), 0), None);
        let mixed = format!("{}{}", prose(2000), "क".repeat(600)); // 23 % not Latin
        assert_eq!(m.observe(&mixed, 900), None, "between 5 % and 30 % it cannot be put to a kind");
        assert_eq!((m.samples(Kind::Prose), m.samples(Kind::NonLatin)), (0, 0));
        // A few typographic characters in an English prompt are still prose.
        let english = format!("{}{}", prose(3000), "’".repeat(30));
        assert_eq!(m.observe(&english, 1000), Some(Kind::Prose));
        // A prompt that is mostly Hindi teaches the Hindi number.
        assert_eq!(m.observe(&hindi(3000), 3000), Some(Kind::NonLatin));
        assert_eq!(m.chars_per_token(Kind::NonLatin), 1.5, "one character a token, but never below one and a half");
        // JSON is code.
        assert_eq!(m.observe(&json(3000), 1200), Some(Kind::Code));
    }

    #[test]
    fn the_room_of_the_rules_follows_the_dearer_of_the_two_latin_rates() {
        let m = TokenMeter::new();
        assert_eq!(m.latin_chars_for(1000), 3000);
        // Prose is learned to be 4 characters a token (believed after three samples) and code 2: the room is counted at 2, so it does not move when the last reply was prose.
        for _ in 0..3 {
            m.observe(&prose(4000), 1000);
            m.observe(&json(2000), 1000);
        }
        assert_eq!((m.chars_per_token(Kind::Prose), m.chars_per_token(Kind::Code)), (4.0, 2.0));
        assert_eq!(m.latin_chars_for(1000), 2000);
        assert_eq!(m.latin_chars_for(0), 0);
    }

    #[test]
    fn a_refusal_only_ever_lowers_the_number() {
        let m = TokenMeter::new();
        // A prompt of 15,000 characters that did not fit 6,144 tokens: it was more than 2.44 characters a token.
        assert_eq!(m.observe_at_least(&prose(15_000), 6145), Some(Kind::Prose));
        assert_eq!(m.chars_per_token(Kind::Prose), 2.44);
        // A refusal that bounds the number higher than it is now (a longer prompt of the same count) changes nothing.
        m.observe_at_least(&prose(30_000), 6145);
        assert_eq!(m.chars_per_token(Kind::Prose), 2.44);
        assert_eq!(m.observe_at_least(&prose(3000), 0), None);
    }

    #[test]
    fn a_test_meter_is_its_own_and_the_process_meter_is_not_moved() {
        let before = meter().chars_per_token(Kind::Prose);
        with_test_meter(|own| {
            own.observe(&prose(3000), 3000);
            assert_eq!(meter().chars_per_token(Kind::Prose), 1.5);
            assert!(std::ptr::eq(meter(), own));
        });
        assert_eq!(meter().chars_per_token(Kind::Prose), before);
        // And it is gone after a panic too.
        let _ = std::panic::catch_unwind(|| with_test_meter(|_| panic!("a test failed")));
        assert_eq!(meter().chars_per_token(Kind::Prose), before);
        // reset() puts the starting numbers back.
        let m = TokenMeter::new();
        m.observe(&json(3000), 3000);
        m.reset();
        assert_eq!((m.chars_per_token(Kind::Code), m.samples(Kind::Code)), (3.0, 0));
    }

    #[test]
    fn the_number_of_tokens_a_refusal_names_is_read_from_the_sentence_that_says_so() {
        let said = "the language model refused this request (HTTP 400: exceeds the available context size (6144 tokens; the prompt has 6234))";
        assert_eq!(refusal_prompt_tokens(said), Some(6234));
        assert_eq!(refusal_prompt_tokens("the language model refused this request (HTTP 400: unsupported response_format)"), None);
        assert_eq!(refusal_prompt_tokens("the prompt has many"), None);
    }
}
