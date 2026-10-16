//! What was read, and finding words in it: the lines the text reader gave, each with the words it is made of and where each word is on the screen.

use super::geometry::Rect;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Word {
    pub text: String,
    #[serde(flatten)]
    pub at: Rect,
}

/// A line of text on the screen. The words are kept for finding a word; they are not sent on with the line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    pub text: String,
    #[serde(flatten)]
    pub at: Rect,
    #[serde(skip)]
    pub words: Vec<Word>,
}

/// What the text reader found in a picture: the language it read with and the lines, in the order it gave them (top to bottom).
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub language: String,
    pub lines: Vec<Line>,
}

/// A place where the words that were asked for are on the screen.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
    /// The words as they are on the screen.
    pub text: String,
    /// The line they are in (an index into the lines that were read).
    pub line: usize,
    #[serde(flatten)]
    pub at: Rect,
    /// The middle of the words, where a click on them would go.
    pub center_x: i32,
    pub center_y: i32,
}

/// A word as it is compared: lower case, with everything that is not a letter or a digit left out ("Save," and "save" are one word).
fn token(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn tokens(text: &str) -> Vec<String> {
    text.split_whitespace().map(token).filter(|t| !t.is_empty()).collect()
}

fn same(wanted: &str, found: &str, loose: bool) -> bool {
    wanted == found || (loose && wanted.chars().count() >= 3 && found.contains(wanted))
}

/// Where in the lines the words of `needle` are, in the order of the lines, at most `max` places. The words must follow each other inside one line. Capitals, the
/// punctuation around a word and the spaces between words do not matter. A single word of three letters or more also finds the words that hold it ("aqua" finds
/// "Aqua's"); several words must each be a word of the line.
pub fn find(lines: &[Line], needle: &str, max: usize) -> Vec<Match> {
    let wanted = tokens(needle);
    if wanted.is_empty() || max == 0 {
        return Vec::new();
    }
    let loose = wanted.len() == 1;
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        // The words of the line that have letters or digits in them, with where each is in the line's list of words.
        let have: Vec<(usize, String)> = line.words.iter().enumerate().map(|(i, w)| (i, token(&w.text))).filter(|(_, t)| !t.is_empty()).collect();
        if have.len() < wanted.len() {
            continue;
        }
        let mut start = 0;
        while start + wanted.len() <= have.len() {
            if wanted.iter().zip(&have[start..]).all(|(w, (_, h))| same(w, h, loose)) {
                let first = have[start].0;
                let last = have[start + wanted.len() - 1].0;
                let words = &line.words[first..=last];
                let at = words.iter().map(|w| w.at).reduce(|a, b| a.union(&b)).unwrap_or(line.at);
                let (center_x, center_y) = at.center();
                out.push(Match { text: words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "), line: index, at, center_x, center_y });
                if out.len() >= max {
                    return out;
                }
                start += wanted.len();
            } else {
                start += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(words: &[(&str, i32)], y: i32) -> Line {
        let ws: Vec<Word> = words.iter().map(|(t, x)| Word { text: (*t).to_string(), at: Rect::new(*x, y, 50, 20) }).collect();
        let at = ws.iter().map(|w| w.at).reduce(|a, b| a.union(&b)).unwrap();
        Line { text: ws.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "), at, words: ws }
    }

    #[test]
    fn words_are_found_by_their_letters_wherever_they_are_and_the_place_is_the_words_not_the_line() {
        let lines = vec![line(&[("File", 10), ("Edit", 70), ("View", 130)], 5), line(&[("Save,", 300), ("then", 360), ("Save", 420), ("As...", 480)], 200), line(&[("The", 10), ("Aqua's", 70), ("window", 130)], 400)];
        let hit = find(&lines, "edit", 5);
        assert_eq!(hit.len(), 1);
        assert_eq!((hit[0].line, hit[0].at, hit[0].center_x, hit[0].center_y), (0, Rect::new(70, 5, 50, 20), 95, 15));
        // Capitals and the punctuation around a word do not matter; the same word twice in a line is twice.
        let saves = find(&lines, "SAVE", 5);
        assert_eq!(saves.iter().map(|m| (m.line, m.at.x)).collect::<Vec<_>>(), vec![(1, 300), (1, 420)]);
        // Several words must follow each other: the place is all of them.
        let two = find(&lines, "save as", 5);
        assert_eq!(two.len(), 1);
        assert_eq!((two[0].text.as_str(), two[0].at), ("Save As...", Rect::new(420, 200, 110, 20)));
        assert!(find(&lines, "as save", 5).is_empty(), "in the order they were asked for");
        assert!(find(&lines, "file view", 5).is_empty(), "and next to each other: Edit is between them");
        // One word of three letters or more also finds a word that holds it; a short one must be the whole word.
        assert_eq!(find(&lines, "aqua", 5).len(), 1);
        assert!(find(&lines, "qu", 5).is_empty() && find(&lines, "ed", 5).is_empty());
        assert_eq!(find(&lines, "view", 1).len(), 1);
        // Nothing asked for, nothing found, and the number of places is held.
        assert!(find(&lines, "", 5).is_empty() && find(&lines, "  ... ", 5).is_empty() && find(&lines, "save", 0).is_empty());
        assert_eq!(find(&lines, "save", 1).len(), 1);
        assert!(find(&lines, "nothing like this", 5).is_empty());
        // A line without words is not searched and does not stop the search.
        let mut with_empty = lines.clone();
        with_empty.insert(0, Line { text: "x".into(), at: Rect::new(0, 0, 1, 1), words: vec![] });
        assert_eq!(find(&with_empty, "edit", 5).len(), 1);
    }

    #[test]
    fn a_number_with_a_colon_or_a_stop_in_it_is_one_word_and_other_alphabets_are_compared_in_lower_case() {
        let lines = vec![line(&[("Meeting", 0), ("at", 60), ("7:30", 120), ("PM", 180)], 0), line(&[("Привет", 0), ("МИР", 60)], 50)];
        assert_eq!(find(&lines, "7.30", 5).len(), 1, "the punctuation inside is left out of both");
        assert_eq!(find(&lines, "приВЕТ мир", 5).len(), 1);
    }
}
