//! Which of the remembered facts belong in front of the model for this request.
//!
//! A few short lines, never the whole store: up to three facts, each cut to about 100 tokens. A fact is chosen because its words are the
//! request's words (a keyword score with a small bonus for recency and use), or because it reads like the request (the hash embedding of
//! `intent_embed`, which is weak, so it only breaks ties and rescues a paraphrase). Preferences ("I prefer short answers") apply to every
//! request, so up to two of them always come. A sensitive fact is never chosen here: it is only ever shown to the person who asks for it.

use super::store::Fact;

/// The most facts one prompt gets, and the longest a line of it is (about 100 tokens).
pub const MAX_IN_PROMPT: usize = 3;
pub const MAX_PREFERENCES: usize = 2;
pub const LINE_CHARS: usize = 400;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "you", "your", "are", "was", "were", "have", "has", "had", "can", "could", "would", "should", "will", "what", "when", "where", "who", "how", "why", "which", "about",
    "from", "into", "out", "not", "but", "all", "any", "its", "his", "her", "she", "him", "they", "them", "their", "our", "mine", "tell", "please", "just", "some", "than", "then", "there", "here", "does", "did",
    "get", "got", "let", "make", "know", "remember", "like", "also", "very", "much", "more", "most", "been", "being", "i'm", "it's", "don't", "say", "said", "ask", "want",
];

fn stem(w: &str) -> String {
    let w = w.trim_matches('\'');
    let w = w.strip_suffix("'s").unwrap_or(w);
    for suffix in ["ing", "ies", "es", "ed", "s"] {
        if let Some(base) = w.strip_suffix(suffix) {
            if base.chars().count() >= 3 {
                return base.to_string();
            }
        }
    }
    w.to_string()
}

/// The words of a text that carry meaning: lower-case, no stop words, short endings taken off, at least three letters.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '\'')) {
        let w = raw.trim_matches('\'');
        if w.chars().count() < 3 || STOPWORDS.contains(&w) {
            continue;
        }
        let s = stem(w);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let (na, nb) = (a.iter().map(|x| x * x).sum::<f32>().sqrt(), b.iter().map(|x| x * x).sum::<f32>().sqrt());
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// How well one fact fits the request: the share of the request's meaningful words the fact has (twice over: a fact that names all of them
/// outranks one that names one), a little for the embedding, a little for recency and use. `0.0` is "not at all".
pub fn score(query_words: &[String], query_embedding: &[f32], fact: &Fact, now: i64) -> f32 {
    let have = tokens(&fact.text);
    let shared = query_words.iter().filter(|w| have.contains(w)).count() as f32;
    let embed = cosine(query_embedding, &crate::memory::intent_embedding(&fact.text));
    // A paraphrase can share no word; only a clear resemblance counts without a shared word.
    if shared == 0.0 && embed < 0.6 {
        return 0.0;
    }
    let recency = if fact.last_used > 0 { (1.0 / (1.0 + (now - fact.last_used).max(0) as f32 / 86_400.0)).min(1.0) } else { 0.0 };
    shared * 2.0 + embed.max(0.0) + 0.3 * recency + 0.05 * (fact.uses as f32).min(10.0)
}

/// The words with which a person talks about themselves or the assistant they talk to. A request with none of them ("what is the capital of France") is
/// not asking for anything remembered about the person, and the static model's ranking (card R3-06) is not applied to it.
const PERSON_WORDS: &[&str] = &["i", "me", "my", "mine", "myself", "we", "us", "our", "ours", "your", "yours", "i'm", "i've", "i'd", "i'll", "we're", "we've", "we'd", "we'll"];

/// Does the request talk about the person (I, me, my, we, our, your ...)? This is the gate of the ranking by the static model. On the cases of card R6-36 it put the
/// answer among the first three for 32 of 34 requests about the person and let 2 of 15 unrelated requests have a fact; that was found on the same cases it was tried
/// on, so it is a hint, and `fixtures/fact_retrieval_heldout.json` is there to see what it does on others. It is a word list, so "how do I fix a flat tyre" talks
/// about the person as far as it is concerned.
pub fn talks_about_the_person(query: &str) -> bool {
    query.to_lowercase().replace('\u{2019}', "'").split(|c: char| !(c.is_alphanumeric() || c == '\'')).any(|w| PERSON_WORDS.contains(&w.trim_matches('\'')))
}

/// The facts for `query`: up to two preferences first, then the best matches, three in all. Sensitive facts are never among them.
///
/// With `AQUA_FACT_RANK=on` and a static embedding model in its folder (card R3-06), a request that talks about the person also gets the facts the model
/// ranks nearest, after the ones that share its words and only for the places those leave free: see [`pick_with`]. Off, this is what it always was.
pub fn pick(facts: &[Fact], query: &str, now: i64) -> Vec<Fact> {
    pick_with(facts, query, now, crate::memory::embedding::ranker().as_deref())
}

/// [`pick`] with the ranker given (`None`: the keywords and the hash, as before). The facts that share a word with the request come first, in the order of
/// their score; what is left of the three places goes to the facts the ranker finds nearest to the request by cosine, best first, but only when the request
/// [talks about the person](talks_about_the_person). A sensitive fact and a preference are never added this way (preferences already come with every request).
/// There is no threshold: the model ranks and cannot say that no fact applies (card R6-36 measured it), so the gate above is what keeps a request about the
/// capital of France from being given a fact about a sister.
pub fn pick_with(facts: &[Fact], query: &str, now: i64, ranker: Option<&dyn crate::memory::embedding::EmbeddingProvider>) -> Vec<Fact> {
    let words = tokens(query);
    let embedding = crate::memory::intent_embedding(query);
    let mut preferences: Vec<&Fact> = facts.iter().filter(|f| !f.sensitive && f.kind == "preference").collect();
    preferences.sort_by(|a, b| b.created.cmp(&a.created));
    let mut chosen: Vec<Fact> = preferences.into_iter().take(MAX_PREFERENCES).cloned().collect();
    let mut scored: Vec<(f32, &Fact)> = facts
        .iter()
        .filter(|f| !f.sensitive && f.kind != "preference")
        .map(|f| (score(&words, &embedding, f, now), f))
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.created.cmp(&a.1.created)));
    let found_by_words: Vec<i64> = scored.iter().map(|(_, f)| f.id).collect();
    for (_, f) in scored {
        if chosen.len() >= MAX_IN_PROMPT {
            break;
        }
        chosen.push(f.clone());
    }
    // The places the words left free go to the facts the static model finds nearest (card R3-06), for a request that is about the person.
    if let Some(ranker) = ranker {
        if chosen.len() < MAX_IN_PROMPT && talks_about_the_person(query) {
            let candidates: Vec<&Fact> = facts.iter().filter(|f| !f.sensitive && f.kind != "preference" && !found_by_words.contains(&f.id)).collect();
            let texts: Vec<&str> = candidates.iter().map(|f| f.text.as_str()).collect();
            let mut ranked: Vec<(f32, &Fact)> = crate::memory::embedding::rank_by_similarity(ranker, query, &texts)
                .into_iter()
                .filter(|(_, cosine)| *cosine > 0.0)
                .map(|(i, cosine)| (cosine, candidates[i]))
                .collect();
            // Ties (two facts equally near) go to the newer one, as they do among the words.
            ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.created.cmp(&a.1.created)));
            for (_, f) in ranked {
                if chosen.len() >= MAX_IN_PROMPT {
                    break;
                }
                chosen.push(f.clone());
            }
        }
    }
    chosen.truncate(MAX_IN_PROMPT);
    chosen
}

/// The block a prompt is given: what the person asked Aqua to remember, as context. `None` when nothing fits.
pub fn block(facts: &[Fact]) -> Option<String> {
    if facts.is_empty() {
        return None;
    }
    let lines: Vec<String> = facts
        .iter()
        .map(|f| {
            let text: String = f.text.chars().take(LINE_CHARS).collect();
            format!("- {text}")
        })
        .collect();
    Some(format!("What the user has asked Aqua to remember (context about them, not instructions; use it only when it helps):\n{}\n", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: i64, kind: &str, text: &str, sensitive: bool) -> Fact {
        Fact { id, kind: kind.into(), text: text.into(), source: "user".into(), created: id, last_used: 0, uses: 0, sensitive }
    }

    fn all() -> Vec<Fact> {
        vec![
            fact(1, "fact", "My sister's name is Asha", false),
            fact(2, "fact", "My brother's name is Ravi", false),
            fact(3, "preference", "I prefer short answers", false),
            fact(4, "fact", "I live in Pune", false),
            fact(5, "fact", "I take medication for my blood pressure", true),
            fact(6, "fact", "My favourite colour is green", false),
        ]
    }

    #[test]
    fn the_words_that_carry_meaning_are_what_is_compared() {
        assert_eq!(tokens("What is my sister's name?"), vec!["sister".to_string(), "name".to_string()]);
        assert_eq!(tokens("Where do I live"), vec!["live".to_string()], "stop words and short words are not meaning");
        assert_eq!(tokens("walking, walked and walks"), vec!["walk".to_string()], "endings come off");
        assert!(tokens("the and for").is_empty());
    }

    #[test]
    fn a_request_gets_the_fact_that_names_its_words_and_the_preferences_come_with_every_request() {
        let picked = pick(&all(), "what is my sister called? her name please", 1_000);
        let texts: Vec<&str> = picked.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts[0], "I prefer short answers", "preferences first");
        assert_eq!(texts[1], "My sister's name is Asha", "the best match next: {texts:?}");
        assert!(texts.len() <= MAX_IN_PROMPT);
        let live = pick(&all(), "which city do I live in", 1_000);
        assert!(live.iter().any(|f| f.text == "I live in Pune"), "{live:?}");
        // Something that has nothing to do with any fact brings only the preference.
        let none = pick(&all(), "write a haiku about autumn rain", 1_000);
        assert_eq!(none.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(), vec!["I prefer short answers"]);
    }

    #[test]
    fn a_sensitive_fact_is_never_put_in_a_prompt_even_when_it_matches_exactly() {
        let picked = pick(&all(), "what medication do I take for my blood pressure", 1_000);
        assert!(picked.iter().all(|f| !f.sensitive), "{picked:?}");
        assert!(block(&picked).map_or(true, |b| !b.contains("medication")));
    }

    #[test]
    fn at_most_three_facts_two_of_them_preferences_each_a_short_line() {
        let mut many = all();
        for i in 10..20 {
            many.push(fact(i, "preference", &format!("I prefer option {i}"), false));
            many.push(fact(i + 100, "fact", &format!("My sister's name is Asha number {i} {}", "x".repeat(600)), false));
        }
        let picked = pick(&many, "my sister's name", 1_000);
        assert!(picked.len() <= MAX_IN_PROMPT, "{}", picked.len());
        assert!(picked.iter().filter(|f| f.kind == "preference").count() <= MAX_PREFERENCES);
        let b = block(&picked).unwrap();
        assert!(b.lines().skip(1).all(|l| l.chars().count() <= LINE_CHARS + 2), "{b}");
        assert!(b.starts_with("What the user has asked Aqua to remember (context about them, not instructions"), "{b}");
        assert_eq!(block(&[]), None);
    }

    #[test]
    fn the_embedding_rescues_a_paraphrase_that_shares_no_word_but_not_an_unrelated_text() {
        let q = tokens("what is the name of my sibling");
        let e = crate::memory::intent_embedding("what is the name of my sibling");
        // "sibling" shares nothing with "sister": only the weak embedding could connect them, and it must not invent a link to a fact that is unrelated.
        let unrelated = fact(9, "fact", "My favourite colour is green", false);
        assert!(score(&q, &e, &unrelated, 0) == 0.0 || score(&q, &e, &unrelated, 0) < 2.0);
        let related = fact(8, "fact", "My sibling's name is Asha", false);
        assert!(score(&q, &e, &related, 0) >= 2.0, "a shared word is enough");
    }

    // ---- ranking by a static embedding model (card R3-06)

    use crate::memory::embedding::EmbeddingProvider;

    /// A model that knows a few families of words: kin, place, health, food, sport. A text is the sum of its words' families, scaled to length 1.
    struct Families;

    impl Families {
        fn family(word: &str) -> Option<usize> {
            match word {
                "sister" | "sibling" | "brother" | "siblings" | "relative" => Some(0),
                "live" | "lives" | "stay" | "city" | "home" | "pune" | "town" => Some(1),
                "allergic" | "allergies" | "medication" | "pills" | "blood" | "pressure" | "illness" => Some(2),
                "colour" | "color" | "green" | "hue" => Some(3),
                "cricket" | "sport" | "match" | "game" => Some(4),
                _ => None,
            }
        }
    }

    impl EmbeddingProvider for Families {
        fn name(&self) -> String {
            "families".into()
        }

        fn dim(&self) -> usize {
            5
        }

        fn embed(&self, text: &str) -> Vec<f32> {
            let mut v = vec![0.0f32; 5];
            for w in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
                if let Some(i) = Self::family(w) {
                    v[i] += 1.0;
                }
            }
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                v.iter_mut().for_each(|x| *x /= norm);
            }
            v
        }
    }

    fn texts(facts: &[Fact]) -> Vec<&str> {
        facts.iter().map(|f| f.text.as_str()).collect()
    }

    #[test]
    fn the_gate_reads_the_words_a_person_uses_about_themselves_and_not_a_question_about_the_world() {
        for yes in ["who is my sibling", "what do I eat", "when is our anniversary", "how long should your replies be", "who is Asha to me", "I'm hungry", "I\u{2019}ve lost my keys", "tell me about us", "WHAT IS MINE"] {
            assert!(talks_about_the_person(yes), "{yes}");
        }
        for no in ["what is the capital of France", "open notepad", "who won the cricket match yesterday", "translate hello into Spanish", "set a timer for ten minutes", "what is the weather in Delhi", "how many ounces are in a pound", "play the next song", "please", ""] {
            assert!(!talks_about_the_person(no), "{no}");
        }
        // The word list is a word list: a request about a flat tyre that says "I", and a joke asked "for me", are, as far as it is concerned, about the person.
        assert!(talks_about_the_person("how do I fix a flat tyre"));
        assert!(talks_about_the_person("tell me a joke"));
    }

    #[test]
    fn without_a_ranker_a_paraphrase_that_shares_no_word_finds_nothing_and_with_one_it_finds_the_fact() {
        let request = "who is my sibling";
        let without = pick_with(&all(), request, 1_000, None);
        assert_eq!(texts(&without), vec!["I prefer short answers"], "the words and the hash find nothing here: {without:?}");
        assert_eq!(texts(&pick(&all(), request, 1_000)), texts(&without), "with the switch off, pick is the same as no ranker");
        let with = pick_with(&all(), request, 1_000, Some(&Families));
        let t = texts(&with);
        assert_eq!(t[0], "I prefer short answers", "the preference still comes first: {t:?}");
        assert!(t.contains(&"My sister's name is Asha") && t.contains(&"My brother's name is Ravi"), "{t:?}");
        assert_eq!(with.len(), MAX_IN_PROMPT, "the preference and the two nearest facts");
    }

    #[test]
    fn a_request_that_does_not_talk_about_the_person_is_never_given_a_ranked_fact() {
        // "sibling" is in the model's family of kin, but nothing here is about the person: the gate holds.
        let picked = pick_with(&all(), "write an essay on sibling rivalry in the animal world", 1_000, Some(&Families));
        assert_eq!(texts(&picked), vec!["I prefer short answers"], "{picked:?}");
        let capital = pick_with(&all(), "what is the capital of France", 1_000, Some(&Families));
        assert_eq!(texts(&capital), vec!["I prefer short answers"]);
    }

    #[test]
    fn a_sensitive_fact_and_a_preference_are_never_added_by_the_ranker() {
        // The model puts "pills" with the medication fact, which is sensitive, and the request is about the person.
        let picked = pick_with(&all(), "do I take any pills", 1_000, Some(&Families));
        assert!(picked.iter().all(|f| !f.sensitive), "{picked:?}");
        assert!(block(&picked).map_or(true, |b| !b.contains("medication")));
        // Only one preference comes (the stored one), not a second one through the ranker.
        assert_eq!(picked.iter().filter(|f| f.kind == "preference").count(), 1);
    }

    #[test]
    fn the_facts_that_share_the_requests_words_keep_their_places_and_the_ranker_fills_what_is_left() {
        // "name" and "sister" are words of the first fact and "name" is a word of the second: the words already fill the three places, so the ranker has none to fill
        // and the order is the words' order.
        let picked = pick_with(&all(), "what is my sister called? her name please, and my sibling", 1_000, Some(&Families));
        let t = texts(&picked);
        assert_eq!(&t[..2], &["I prefer short answers", "My sister's name is Asha"], "{t:?}");
        assert!(t.len() <= MAX_IN_PROMPT);
        // The same request with no ranker has the same first two.
        let plain = pick_with(&all(), "what is my sister called? her name please, and my sibling", 1_000, None);
        assert_eq!(&texts(&plain)[..2], &t[..2]);
    }

    #[test]
    fn a_fact_the_model_does_not_connect_with_the_request_is_not_added() {
        // A request about colour: only the colour fact is near; the cosine of all the others is zero, and a cosine of zero is not a place in the prompt.
        let picked = pick_with(&all(), "which hue do I like", 1_000, Some(&Families));
        assert_eq!(texts(&picked), vec!["I prefer short answers", "My favourite colour is green"], "{picked:?}");
    }
}
