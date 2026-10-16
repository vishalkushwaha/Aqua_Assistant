//! Cleaning up what was dictated, by rules and nothing else: no model is asked, so it is instant and the same text always gives the same result.
//!
//! "Light" does what a careful typist would: it takes out filler sounds and stutters, follows a spoken correction ("scratch that", "at 3, actually 4"),
//! puts in the person's own word replacements, fixes spacing and capital letters, and expands snippets. A rule that cannot be sure of what the person
//! meant does nothing (a "never mind" in the middle of a sentence stays a "never mind"): a wrong edit is worse than none, because the text goes into
//! somebody's document without them looking at it first.

use super::{Level, Settings};
use regex::{Regex, RegexBuilder};

/// What came out, and what was done to get there (for the person to see, and for tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    pub text: String,
    pub applied: Vec<String>,
}

const FILLERS: [&str; 10] = ["um", "umm", "uh", "uhh", "er", "erm", "ah", "hm", "hmm", "mm"];
/// A filler that is also a word ("the ER", "she works in er"): only taken out when it stands alone, between commas or pauses.
const FILLERS_THAT_ARE_WORDS: [&str; 1] = ["er"];
/// Words that are only ever said twice by stumbling (a stutter), never on purpose ("had had", "that that" and "so so" can be real, so they are not here).
const STUTTER_SAFE: [&str; 17] = ["the", "a", "an", "to", "of", "in", "on", "and", "i", "it", "we", "but", "or", "for", "with", "at", "by"];
/// Words that end in a full stop without ending a sentence (lower case).
const ABBREVIATIONS: [&str; 15] = ["i.e.", "e.g.", "etc.", "vs.", "dr.", "mr.", "mrs.", "ms.", "a.m.", "p.m.", "prof.", "sr.", "jr.", "st.", "approx."];
const UNDO_PHRASES: [&[&str]; 4] = [&["scratch", "that"], &["delete", "that"], &["cancel", "that"], &["never", "mind"]];

fn bare(token: &str) -> String {
    token.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase()
}

/// An abbreviation that ends in a full stop without ending a sentence: "i.e.", "e.g.", "Dr.", "a.m.".
fn is_abbreviation(token: &str) -> bool {
    let word = token.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    ABBREVIATIONS.contains(&word.as_str())
}

fn ends_sentence(token: &str) -> bool {
    token.ends_with(['.', '?', '!']) && !is_abbreviation(token)
}

fn trailing_punct(token: &str) -> &str {
    let end = token.trim_end_matches(|c: char| !c.is_alphanumeric()).len();
    &token[end..]
}

fn is_number(word: &str) -> bool {
    static NUMBER: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    NUMBER.get_or_init(|| Regex::new(r"^(\d{1,2}(:\d{2})?(am|pm|a\.m|p\.m)?|\d+([.,]\d+)?)$").expect("a valid pattern")).is_match(word)
}

/// "scratch that" and its kin, when they are the end of a clause: the sentence being said is dropped. Returns how many phrases were followed.
fn follow_undo(tokens: Vec<String>, applied: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut undone = 0;
    while i < tokens.len() {
        // The phrase is a clause of its own: it begins one (the start of the text, or a stop, comma or pause before it) and ends one (punctuation after it, or nothing).
        // In the middle of a sentence it is ordinary speech: "I told him to cancel that." and "please delete that." are what was said.
        let begins_a_clause = i == 0 || out.last().is_some_and(|t| t.ends_with(['.', '?', '!', ',', ';', ':']));
        let hit = UNDO_PHRASES.iter().find(|p| {
            begins_a_clause
                && i + p.len() <= tokens.len()
                && p.iter().enumerate().all(|(k, w)| bare(&tokens[i + k]) == *w)
                && (i + p.len() == tokens.len() || !trailing_punct(&tokens[i + p.len() - 1]).is_empty())
        });
        match hit {
            Some(p) => {
                // Said on its own after a full stop, it takes back the sentence before it; said at the end of a clause, the sentence being said.
                if out.last().is_some_and(|t| ends_sentence(t)) {
                    out.pop();
                }
                while out.last().is_some_and(|t| !ends_sentence(t)) {
                    out.pop();
                }
                i += p.len();
                undone += 1;
            }
            None => {
                out.push(tokens[i].clone());
                i += 1;
            }
        }
    }
    if undone > 0 {
        applied.push(format!("followed {undone} \"scratch that\""));
    }
    out
}

/// "at 3, actually 4": a number said again with "actually" in front of it replaces the number before it. Only numbers: for anything else the
/// word that is taken back is not certain.
fn follow_actually(tokens: Vec<String>, applied: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut fixed = 0;
    while i < tokens.len() {
        let is_fix = bare(&tokens[i]) == "actually" && i + 1 < tokens.len() && is_number(&bare(&tokens[i + 1])) && out.last().is_some_and(|prev| is_number(&bare(prev)));
        if is_fix {
            out.pop();
            fixed += 1;
            i += 1;
        } else {
            out.push(tokens[i].clone());
            i += 1;
        }
    }
    if fixed > 0 {
        applied.push(format!("followed {fixed} \"actually\" correction(s) of a number"));
    }
    out
}

fn drop_fillers(tokens: Vec<String>, applied: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut removed = 0;
    for t in tokens {
        let word = bare(&t);
        let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
        // "ER" and "UM" in capitals are what they say (an emergency room, a university), not a sound.
        let shouted = letters.len() >= 2 && letters.iter().all(|c| c.is_uppercase());
        let alone = out.last().is_none_or(|p| p.ends_with([',', ';', ':', '.', '?', '!']));
        if FILLERS.contains(&word.as_str()) && !shouted && (alone || !FILLERS_THAT_ARE_WORDS.contains(&word.as_str())) {
            removed += 1;
            // A full stop after the filler still ends the sentence.
            let punct = trailing_punct(&t);
            if punct.starts_with(['.', '?', '!']) {
                if let Some(last) = out.last_mut() {
                    if !last.ends_with(['.', '?', '!', ',']) {
                        last.push_str(&punct[..1]);
                    }
                }
            }
            continue;
        }
        out.push(t);
    }
    if removed > 0 {
        applied.push(format!("removed {removed} filler sound(s)"));
    }
    out
}

fn drop_stutters(tokens: Vec<String>, applied: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut removed = 0;
    for t in tokens {
        let word = bare(&t);
        let same_as_last = out.last().is_some_and(|prev| bare(prev) == word && !trailing_punct(prev).contains(['.', '?', '!']));
        if same_as_last && STUTTER_SAFE.contains(&word.as_str()) {
            removed += 1;
            if let Some(last) = out.last_mut() {
                *last = t;
            }
            continue;
        }
        out.push(t);
    }
    if removed > 0 {
        applied.push(format!("removed {removed} repeated word(s)"));
    }
    out
}

fn phrase_regex(phrase: &str) -> Option<Regex> {
    let words: Vec<String> = phrase.split_whitespace().map(regex::escape).collect();
    if words.is_empty() {
        return None;
    }
    // Not inside a longer word, and not part of an address or a number (a letter, a digit, a dot or an @ right next to it).
    RegexBuilder::new(&format!(r"(?:^|[^\w@.])({})(?:[^\w@]|$)", words.join(r"\s+"))).case_insensitive(true).build().ok()
}

/// Replaces every whole-word occurrence of `from` (any capitalisation) with `to` exactly as written. Returns how many were replaced.
fn replace_phrase(text: &mut String, from: &str, to: &str) -> usize {
    let Some(re) = phrase_regex(from) else { return 0 };
    let mut count = 0;
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    // The pattern consumes one character on each side so that it can tell a word from a piece of one; the match inside it is what is replaced.
    while let Some(cap) = re.captures(&text[at..]) {
        let m = cap.get(1).expect("the phrase is group 1");
        out.push_str(&text[at..at + m.start()]);
        out.push_str(to);
        at += m.end();
        count += 1;
    }
    out.push_str(&text[at..]);
    *text = out;
    count
}

/// "word ." is "word.", but a stop that starts a word keeps its space: "edit the .env file", "I like .NET".
fn close_up_stops(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let starts_a_word = chars.get(i + 2).is_some_and(|c| c.is_alphanumeric());
        if chars[i] == ' ' && chars.get(i + 1) == Some(&'.') && !starts_a_word {
            i += 1; // the space goes, the stop stays
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Spaces before punctuation go, a lone "i" becomes "I", and the first letter of the text and of every sentence is a capital.
fn tidy(text: &str) -> String {
    let mut s = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for p in [" ,", " ;", " :", " !", " ?"] {
        s = s.replace(p, &p[1..]);
    }
    s = close_up_stops(&s);
    static COMMAS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    s = COMMAS.get_or_init(|| Regex::new(r",\s*,+").expect("a valid pattern")).replace_all(&s, ",").into_owned();
    static LONE_I: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let lone_i = LONE_I.get_or_init(|| Regex::new(r"\bi\b(?:'(m|ll|ve|d))?").expect("a valid pattern"));
    let source = s.clone();
    s = lone_i
        .replace_all(&source, |c: &regex::Captures| {
            let whole = c.get(0).map_or("", |m| m.as_str());
            // The "i" of "i.e." (a letter after the stop) is not the word "I".
            let end = c.get(0).map_or(0, |m| m.end());
            if source[end..].starts_with('.') && source[end + 1..].chars().next().is_some_and(char::is_alphabetic) {
                return whole.to_string();
            }
            match c.get(1) {
                Some(rest) => format!("I'{}", rest.as_str()),
                None => "I".to_string(),
            }
        })
        .into_owned();
    let mut out = String::with_capacity(s.len());
    let mut capital = true;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if capital && ch.is_alphabetic() {
            out.extend(ch.to_uppercase());
            capital = false;
        } else {
            out.push(ch);
            if matches!(ch, '.' | '?' | '!') {
                // A stop ends a sentence only when a space (or the end) follows it: not in "3.5", "me@sig.com" or "e.g.x", and not after "i.e.", "Dr." or "a.m.".
                let word = out.rsplit(char::is_whitespace).next().unwrap_or("");
                capital = chars.peek().is_none_or(|n| n.is_whitespace()) && !(ch == '.' && is_abbreviation(word));
            } else if ch.is_alphanumeric() {
                capital = false;
            }
        }
    }
    out
}

/// A decimal or an address keeps its dot: a capital after a dot is only made when a space follows it (see `tidy`), and `capital` is only set by a dot
/// that ends a word and is followed by a space, which `split_whitespace` and the scan above give.
pub fn clean(text: &str, settings: &Settings) -> Cleaned {
    let mut applied = Vec::new();
    let trimmed = text.trim();
    if settings.cleanup == Level::None || trimmed.is_empty() {
        return Cleaned { text: trimmed.to_string(), applied };
    }
    let mut tokens: Vec<String> = trimmed.split_whitespace().map(str::to_string).collect();
    tokens = follow_undo(tokens, &mut applied);
    tokens = follow_actually(tokens, &mut applied);
    tokens = drop_fillers(tokens, &mut applied);
    tokens = drop_stutters(tokens, &mut applied);
    let mut out = tokens.join(" ");
    for r in &settings.replacements {
        let n = replace_phrase(&mut out, &r.from, &r.to);
        if n > 0 {
            applied.push(format!("replaced \"{}\" with \"{}\" ({n})", r.from, r.to));
        }
    }
    let mut out = tidy(&out);
    for sn in &settings.snippets {
        let n = replace_phrase(&mut out, &sn.trigger, &sn.text);
        if n > 0 {
            applied.push(format!("expanded the snippet \"{}\" ({n})", sn.trigger));
        }
    }
    Cleaned { text: out, applied }
}

#[cfg(test)]
mod tests {
    use super::super::{Replacement, Snippet};
    use super::*;

    fn light() -> Settings {
        Settings::default()
    }

    fn c(text: &str) -> String {
        clean(text, &light()).text
    }

    #[test]
    fn fillers_and_stutters_go_and_the_sentence_is_capitalised_and_tidy() {
        assert_eq!(c("um so i think uh we should go to the the market ."), "So I think we should go to the market.");
        assert_eq!(c("hmm. it works"), "It works", "{}", c("hmm. it works"));
        assert_eq!(c("i'm sure i'll be there and i've said so , er , twice"), "I'm sure I'll be there and I've said so, twice");
        // A word said twice on purpose stays.
        assert_eq!(c("she had had enough"), "She had had enough");
        assert_eq!(c("that that is true"), "That that is true");
        // The words of the filler list inside other words are not touched.
        assert_eq!(c("the summer was humming"), "The summer was humming");
        // A full stop that came after a filler still ends the sentence.
        assert_eq!(c("it is done uh. next one"), "It is done. Next one");
    }

    #[test]
    fn scratch_that_takes_back_the_sentence_being_said_and_only_when_it_ends_a_clause() {
        assert_eq!(c("I will be there at five. Scratch that. I will be late."), "I will be late.");
        assert_eq!(c("Send it to Anna. Send it to Bo, scratch that"), "Send it to Anna.");
        assert_eq!(c("buy milk. never mind. buy bread"), "Buy bread");
        assert_eq!(c("delete that"), "");
        // In the middle of a sentence it is ordinary speech.
        assert_eq!(c("please never mind the cost of it"), "Please never mind the cost of it");
        assert_eq!(c("scratch that itch for me"), "Scratch that itch for me");
        // Only the sentence being said goes.
        assert_eq!(c("First point. Second point. Scratch that. Third point."), "First point. Third point.");
        assert_eq!(c("one. two. three, scratch that. four"), "One. Two. Four", "said at the end of a clause it takes back the sentence being said");
    }

    #[test]
    fn actually_takes_back_a_number_and_nothing_else() {
        assert_eq!(c("let's meet at 3, actually 4."), "Let's meet at 4.");
        assert_eq!(c("it costs 12.50 actually 15"), "It costs 15");
        assert_eq!(c("we start at 3pm, actually 4:30pm"), "We start at 4:30pm");
        // Not about numbers: it is an ordinary word.
        assert_eq!(c("it was actually quite good"), "It was actually quite good");
        assert_eq!(c("I met Anna, actually Bo, at the shop"), "I met Anna, actually Bo, at the shop");
        assert_eq!(c("he is actually 40"), "He is actually 40", "no number before it");
    }

    #[test]
    fn the_persons_replacements_apply_to_whole_words_in_any_capitalisation_and_keep_their_own_spelling() {
        let s = Settings {
            replacements: vec![Replacement { from: "kubernetes".into(), to: "Kubernetes".into() }, Replacement { from: "jay son".into(), to: "JSON".into() }, Replacement { from: "cat".into(), to: "CAT".into() }],
            ..Settings::default()
        };
        let out = clean("deploy it on KUBERNETES and send the jay son to the cat, not the category or concatenate", &s);
        assert_eq!(out.text, "Deploy it on Kubernetes and send the JSON to the CAT, not the category or concatenate");
        assert_eq!(out.applied.iter().filter(|a| a.starts_with("replaced")).count(), 3, "{:?}", out.applied);
    }

    #[test]
    fn a_snippet_is_expanded_exactly_as_written_and_not_inside_an_address_or_a_longer_word() {
        let s = Settings {
            snippets: vec![Snippet { trigger: "my email".into(), text: "anna.k@example.com".into() }, Snippet { trigger: "sig".into(), text: "Best regards,\nAnna".into() }],
            ..Settings::default()
        };
        assert_eq!(clean("write to me at my email please", &s).text, "Write to me at anna.k@example.com please");
        assert_eq!(clean("my email", &s).text, "anna.k@example.com", "a capital is not forced on an expansion");
        assert_eq!(clean("a signature is not a sig", &s).text, "A signature is not a Best regards,\nAnna");
        assert_eq!(clean("mail me@sig.com", &s).text, "Mail me@sig.com", "not inside an address");
    }

    /// Text that is right as it stands comes back as it is, and text with fillers comes back tidy: the lines are in `golden.txt`.
    #[test]
    fn the_golden_lines_come_back_as_the_file_says() {
        let (mut unchanged, mut tidied, mut wrong) = (0, 0, Vec::new());
        let mut section = "";
        for line in include_str!("golden.txt").lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if line == "[unchanged]" || line == "[tidy]" {
                section = line;
                continue;
            }
            match section {
                "[unchanged]" => {
                    unchanged += 1;
                    let got = c(line);
                    if got != line {
                        wrong.push(format!("{line:?} came back as {got:?}"));
                    }
                }
                "[tidy]" => {
                    tidied += 1;
                    let (heard, wanted) = line.split_once(" -> ").expect("a tidy line is `heard -> wanted`");
                    let got = c(heard);
                    if got != wanted {
                        wrong.push(format!("{heard:?} came back as {got:?}, wanted {wanted:?}"));
                    }
                }
                _ => panic!("a line outside a section: {line}"),
            }
        }
        assert!(unchanged >= 200 && tidied >= 100, "{unchanged} lines that must not change, {tidied} that must be tidied");
        assert!(wrong.is_empty(), "{} of {} lines are wrong:\n{}", wrong.len(), unchanged + tidied, wrong.join("\n"));
    }

    #[test]
    fn an_undo_phrase_in_the_middle_of_a_sentence_and_an_acronym_that_looks_like_a_filler_are_left_alone() {
        assert_eq!(c("I told him to cancel that."), "I told him to cancel that.");
        assert_eq!(c("She works in the ER."), "She works in the ER.");
        assert_eq!(c("a hash, i.e. a digest"), "A hash, i.e. a digest");
        assert_eq!(c("edit the .env file ."), "Edit the .env file.");
        // The ones that are meant: a clause of their own.
        assert_eq!(c("I will go, scratch that"), "", "the sentence being said goes");
        assert_eq!(c("We start at nine. cancel that. We start at ten."), "We start at ten.");
        // "er" is a sound only between commas or pauses.
        assert_eq!(c("it is, er, fine"), "It is, fine");
        assert_eq!(c("she works in er"), "She works in er");
    }

    #[test]
    fn with_cleanup_off_only_the_ends_are_trimmed_and_nothing_is_replaced() {
        let s = Settings { cleanup: Level::None, replacements: vec![Replacement { from: "um".into(), to: "x".into() }], ..Settings::default() };
        let out = clean("  um so i think  ", &s);
        assert_eq!((out.text.as_str(), out.applied.len()), ("um so i think", 0));
        assert_eq!(clean("   ", &light()).text, "");
    }

    #[test]
    fn it_says_what_it_did_and_whatever_the_text_it_never_panics() {
        let out = clean("um the the plan. scratch that. do it at 3, actually 4", &light());
        assert_eq!(out.text, "Do it at 4");
        assert!(out.applied.iter().any(|a| a.contains("scratch that")) && out.applied.iter().any(|a| a.contains("actually")), "{:?}", out.applied);
        let mut state: u64 = 0x9e3779b97f4a7c15;
        let words = ["um", "the", "scratch", "that.", "never", "mind", "actually", "3,", "4", "i", "i'm", ".", ",", "my", "email", "😀", "é", "kubernetes", "\n"];
        for _ in 0..2_000 {
            let mut text = String::new();
            for _ in 0..(state % 12) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                text.push_str(words[(state >> 33) as usize % words.len()]);
                text.push(' ');
            }
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let s = Settings { replacements: vec![Replacement { from: "the".into(), to: "THE".into() }], snippets: vec![Snippet { trigger: "my email".into(), text: "x@y.z".into() }], ..Settings::default() };
            let _ = clean(&text, &s);
        }
    }
}
