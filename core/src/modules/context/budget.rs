//! Token budgets for the regions of a planner prompt. The model's own tokenizer is not asked (that needs the server running): a text is
//! counted by its length, three characters to a token, which is a little more than English prose costs and about what JSON and code cost, so
//! a budget kept by this count is kept by the model's too.

/// A rough count of tokens.
pub fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

/// `text` cut to at most `max_tokens`: short enough as it is, or its start and its end with a mark between (the end of a command's output is
/// where its verdict is, and the start says what it was).
pub fn cut_to_tokens(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(3);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars < 60 {
        return text.chars().take(max_chars).collect();
    }
    let mark = " … [cut] … ";
    let room = max_chars.saturating_sub(mark.chars().count());
    let head: String = text.chars().take(room * 2 / 3).collect();
    let tail: String = text.chars().rev().take(room - room * 2 / 3).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}{mark}{tail}")
}

/// What is kept free in the window for what the estimate gets wrong: a tenth of the window, and at least 160 tokens (card OA4-02).
pub fn margin_tokens(context_tokens: u32) -> usize {
    (context_tokens / 10).max(160) as usize
}

/// The most tokens the prompt may take in a window of `context_tokens` when the model may write `reply_tokens`: the window less the reply and the margin. The prompt and the
/// reply share the window, so a prompt that took all of it would leave the reply no room (a code edit may use 640 tokens) and the server would cut the reply off or refuse the request.
/// At most half of the window is kept back, however large the reply: a window so small that the reply and the margin would take more than that (a server started with four
/// cache slots on a 2,048-token window has 512 tokens each) cannot honour both, and a prompt with no room is worse than a reply that may be cut off.
pub fn prompt_tokens_allowed(context_tokens: u32, reply_tokens: u32) -> usize {
    let window = context_tokens as usize;
    window - (reply_tokens as usize + margin_tokens(context_tokens)).min(window / 2)
}

impl Regions {
    /// The characters a prompt of this `kind` may have in a window of `context_tokens` when the model may write `reply_tokens`: [`prompt_tokens_allowed`] at the number of characters a
    /// token costs that the meter has learned for the kind (card OA4-02, task CX-03).
    pub fn fit_chars(context_tokens: u32, reply_tokens: u32, kind: super::meter::Kind) -> usize {
        super::meter::meter().chars_for(prompt_tokens_allowed(context_tokens, reply_tokens), kind)
    }
}

/// What each part of the prompt may use, for a model window of `context_tokens` (6,144 on this machine). The instructions and the list of tools
/// come first and are the same from round to round (the model keeps them in its cache); results, newest last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regions {
    /// The rules and the list of tools.
    pub instructions: usize,
    /// What was learned from earlier attempts.
    pub lessons: usize,
    /// The plan line at the end.
    pub plan: usize,
    /// The newest tool result, in full.
    pub newest: usize,
    /// Each of the older results that are still shown.
    pub older_each: usize,
    /// How many older results are still shown (the rest are only a line with a handle).
    pub keep_older: usize,
    /// Results leave the shown ones, and the handle lines are folded, in blocks of this many, so the start of the prompt is the same for many
    /// rounds in a row.
    pub block: usize,
    /// How many handle-only lines are kept before the oldest block is folded into one line.
    pub handle_lines: usize,
    /// Left empty, in percent of the window.
    pub margin_percent: usize,
}

impl Regions {
    /// The budgets for a window: those of a 6,144-token window, scaled for a bigger or a smaller one (never below what makes sense).
    pub fn for_window(context_tokens: u32) -> Self {
        let scale = |base: usize| (base * context_tokens.clamp(2048, 32768) as usize / 6144).max(40);
        Self { instructions: scale(1400), lessons: scale(300), plan: scale(150), newest: scale(900), older_each: scale(120).max(60), keep_older: 5, block: 4, handle_lines: 12, margin_percent: 10 }
    }

    /// How many characters of lines of text (a page of a file, the lines of a search) fit what the model is shown of the newest result in a window of `context_tokens`: that result's
     /// budget at three characters a token, less 300 characters for the header and the notes around the lines (card OA5-03, CU-03). 2,400 at 6,144; never below 500.
    pub fn view_chars(context_tokens: u32) -> usize {
        (Self::for_window(context_tokens).newest * 3).saturating_sub(300).max(500)
    }

    /// The most the results may take: the window less the reply, the margin and the fixed regions.
    pub fn history_tokens(&self, context_tokens: u32, reply_reserve: usize) -> usize {
        let window = context_tokens as usize;
        let margin = window * self.margin_percent / 100;
        window.saturating_sub(reply_reserve + margin + self.instructions + self.lessons + self.plan + 120)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_is_counted_by_its_length_and_cut_to_a_budget_keeping_both_ends() {
        assert_eq!(tokens(""), 0);
        assert_eq!(tokens("abc"), 1);
        assert_eq!(tokens("abcd"), 2);
        assert_eq!(cut_to_tokens("short", 100), "short");
        let long = format!("START {} END", "x".repeat(5000));
        let cut = cut_to_tokens(&long, 100);
        assert!(cut.chars().count() <= 300 && tokens(&cut) <= 100, "{}", cut.chars().count());
        assert!(cut.starts_with("START") && cut.ends_with("END") && cut.contains("[cut]"), "{cut}");
        assert_eq!(cut_to_tokens("abcdefghij", 2).chars().count(), 6, "a budget too small for a mark is a plain cut");
        // Characters are not split.
        assert!(cut_to_tokens(&"é".repeat(2000), 50).chars().all(|c| c == 'é' || "… []cut".contains(c) || c == ' '));
    }

    #[test]
    fn the_regions_are_those_of_the_card_for_the_window_of_this_machine_and_scale_with_the_window() {
        let r = Regions::for_window(6144);
        assert_eq!((r.instructions, r.lessons, r.plan, r.newest, r.older_each, r.keep_older, r.block), (1400, 300, 150, 900, 120, 5, 4));
        let big = Regions::for_window(12288);
        assert_eq!((big.instructions, big.newest, big.older_each), (2800, 1800, 240));
        let tiny = Regions::for_window(100);
        assert!(tiny.newest >= 40 && tiny.older_each >= 60, "{tiny:?}");
        // What is left for the results: the window less the reply, the margin and the fixed regions.
        assert_eq!(r.history_tokens(6144, 600), 6144 - 600 - 614 - 1400 - 300 - 150 - 120);
        assert_eq!(r.history_tokens(1000, 900), 0, "never negative");
    }

    #[test]
    fn a_page_of_a_file_fits_what_the_model_is_shown_of_the_newest_result_less_the_room_for_its_header() {
        assert_eq!((Regions::view_chars(6144), Regions::view_chars(12288), Regions::view_chars(2048)), (2400, 5100, 600));
        // The newest result is 900 tokens (2,700 characters) in the window of this machine: the page, the header line of about 140 characters and a note all fit in it.
        assert!(Regions::view_chars(6144) + 300 <= Regions::for_window(6144).newest * 3);
        assert_eq!(Regions::view_chars(100), Regions::view_chars(2048), "a window is never taken for less than the smallest the server starts with");
        assert!(Regions::view_chars(32768) <= aqua_tool_host::output::FS_READ_MAX_PAGE_CHARS * 3, "and a very large window is still a page the host does not cut");
    }

    #[test]
    fn the_prompt_may_take_the_window_less_the_reply_and_a_tenth_for_what_the_estimate_gets_wrong() {
        // A tenth of the window, at least 160 tokens.
        assert_eq!((margin_tokens(2048), margin_tokens(6144), margin_tokens(8192), margin_tokens(1000)), (204, 614, 819, 160));
        // A code edit may write 640 tokens, an answer 320.
        assert_eq!((prompt_tokens_allowed(6144, 640), prompt_tokens_allowed(6144, 320)), (4890, 5210));
        assert_eq!((prompt_tokens_allowed(2048, 640), prompt_tokens_allowed(2048, 320)), (1204, 1524));
        assert_eq!(prompt_tokens_allowed(8192, 640), 6733);
        // Prompt, reply and margin always add up to the window.
        for window in [2048u32, 4096, 6144, 8192, 32768] {
            for reply in [0u32, 320, 640] {
                assert_eq!(prompt_tokens_allowed(window, reply) + reply as usize + margin_tokens(window), window as usize);
            }
        }
        // A window too small for the reply and the margin keeps half for the prompt, and is never negative.
        assert_eq!((prompt_tokens_allowed(1000, 900), prompt_tokens_allowed(512, 320), prompt_tokens_allowed(0, 640)), (500, 256, 0));
        assert_eq!(prompt_tokens_allowed(1536, 640), 768, "the four-slot window of a 6,144 server keeps half for the prompt");
        assert_eq!(prompt_tokens_allowed(3072, 640), 3072 - 640 - 307, "the two-slot window keeps its whole reserve");
    }

    #[test]
    fn the_characters_a_prompt_may_have_follow_what_the_meter_has_learned() {
        use super::super::meter::{with_test_meter, Kind};
        with_test_meter(|m| {
            // Before any sample: three characters a token (one and a half for text that is not Latin).
            assert_eq!(Regions::fit_chars(6144, 640, Kind::Prose), 4890 * 3);
            assert_eq!(Regions::fit_chars(6144, 640, Kind::NonLatin), 4890 * 3 / 2);
            // The server counts 2 characters a token in code: the same window holds fewer characters.
            m.observe(&"{\"a\":[1,2],\"b\":(3)}".repeat(40), 380);
            assert!(Regions::fit_chars(6144, 640, Kind::Code) < 4890 * 3, "{}", Regions::fit_chars(6144, 640, Kind::Code));
        });
    }
}
